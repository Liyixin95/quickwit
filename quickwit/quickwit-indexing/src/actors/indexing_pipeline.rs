// Copyright (C) 2022 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use quickwit_actors::{
    create_mailbox, Actor, ActorContext, ActorExitStatus, ActorHandle, Handler, Health, Mailbox,
    QueueCapacity, Supervisable,
};
use quickwit_common::KillSwitch;
use quickwit_config::{IndexingSettings, SourceConfig};
use quickwit_doc_mapper::DocMapper;
use quickwit_metastore::{Metastore, MetastoreError};
use quickwit_storage::Storage;
use tokio::join;
use tokio::sync::Semaphore;
use tracing::{debug, error, info, instrument};

use super::MergePlanner;
use crate::actors::doc_processor::DocProcessor;
use crate::actors::index_serializer::IndexSerializer;
use crate::actors::publisher::PublisherType;
use crate::actors::sequencer::Sequencer;
use crate::actors::uploader::UploaderType;
use crate::actors::{Indexer, Packager, Publisher, Uploader};
use crate::models::{IndexingDirectory, IndexingPipelineId, IndexingStatistics, Observe};
use crate::source::{quickwit_supported_sources, SourceActor, SourceExecutionContext};
use crate::split_store::IndexingSplitStore;
use crate::SplitsUpdateMailbox;

const MAX_RETRY_DELAY: Duration = Duration::from_secs(600); // 10 min.

/// Calculates the wait time based on retry count.
// retry_count, wait_time
// 0   2s
// 1   4s
// 2   8s
// 3   16s
// ...
// >=8   5mn
pub(crate) fn wait_duration_before_retry(retry_count: usize) -> Duration {
    // Protect against a `retry_count` that will lead to an overflow.
    let max_power = (retry_count as u32 + 1).min(31);
    Duration::from_secs(2u64.pow(max_power) as u64).min(MAX_RETRY_DELAY)
}

/// Spawning an indexing pipeline puts a lot of pressure on the file system, metastore, etc. so
/// we rely on this semaphore to limit the number of indexing pipelines that can be spawned
/// concurrently.
/// See also <https://github.com/quickwit-oss/quickwit/issues/1638>.
static SPAWN_PIPELINE_SEMAPHORE: Semaphore = Semaphore::const_new(10);

pub struct IndexingPipelineHandles {
    pub source: ActorHandle<SourceActor>,
    pub doc_processor: ActorHandle<DocProcessor>,
    pub indexer: ActorHandle<Indexer>,
    pub index_serializer: ActorHandle<IndexSerializer>,
    pub packager: ActorHandle<Packager>,
    pub uploader: ActorHandle<Uploader>,
    pub sequencer: ActorHandle<Sequencer<Publisher>>,
    pub publisher: ActorHandle<Publisher>,
}

// Messages

#[derive(Clone, Copy, Debug)]
pub struct Supervise;

#[derive(Clone, Copy, Debug, Default)]
pub struct Spawn {
    retry_count: usize,
}

pub struct IndexingPipeline {
    params: IndexingPipelineParams,
    previous_generations_statistics: IndexingStatistics,
    statistics: IndexingStatistics,
    handles: Option<IndexingPipelineHandles>,
    // Killswitch used for the actors in the pipeline. This is not the supervisor killswitch.
    kill_switch: KillSwitch,
}

#[async_trait]
impl Actor for IndexingPipeline {
    type ObservableState = IndexingStatistics;

    fn observable_state(&self) -> Self::ObservableState {
        self.statistics.clone()
    }

    fn name(&self) -> String {
        "IndexingPipeline".to_string()
    }

    async fn initialize(&mut self, ctx: &ActorContext<Self>) -> Result<(), ActorExitStatus> {
        self.handle(Spawn::default(), ctx).await?;
        self.handle(Observe, ctx).await?;
        self.handle(Supervise, ctx).await?;
        Ok(())
    }
}

impl IndexingPipeline {
    pub fn new(params: IndexingPipelineParams) -> Self {
        Self {
            params,
            previous_generations_statistics: Default::default(),
            handles: None,
            kill_switch: KillSwitch::default(),
            statistics: IndexingStatistics::default(),
        }
    }

    fn supervisables(&self) -> Vec<&dyn Supervisable> {
        if let Some(handles) = &self.handles {
            let supervisables: Vec<&dyn Supervisable> = vec![
                &handles.source,
                &handles.doc_processor,
                &handles.indexer,
                &handles.index_serializer,
                &handles.packager,
                &handles.uploader,
                &handles.sequencer,
                &handles.publisher,
            ];
            supervisables
        } else {
            Vec::new()
        }
    }

    /// Performs healthcheck on all of the actors in the pipeline,
    /// and consolidates the result.
    fn healthcheck(&self) -> Health {
        let mut healthy_actors: Vec<&str> = Default::default();
        let mut failure_or_unhealthy_actors: Vec<&str> = Default::default();
        let mut success_actors: Vec<&str> = Default::default();
        for supervisable in self.supervisables() {
            match supervisable.health() {
                Health::Healthy => {
                    // At least one other actor is running.
                    healthy_actors.push(supervisable.name());
                }
                Health::FailureOrUnhealthy => {
                    failure_or_unhealthy_actors.push(supervisable.name());
                }
                Health::Success => {
                    success_actors.push(supervisable.name());
                }
            }
        }

        if !failure_or_unhealthy_actors.is_empty() {
            error!(
                pipeline_id=?self.params.pipeline_id,
                generation=self.generation(),
                healthy_actors=?healthy_actors,
                failed_or_unhealthy_actors=?failure_or_unhealthy_actors,
                success_actors=?success_actors,
                "Indexing pipeline failure."
            );
            return Health::FailureOrUnhealthy;
        }
        if healthy_actors.is_empty() {
            // All the actors finished successfully.
            info!(
                pipeline_id=?self.params.pipeline_id,
                generation=self.generation(),
                "Indexing pipeline success."
            );
            return Health::Success;
        }
        // No error at this point and there are still some actors running.
        debug!(
            pipeline_id=?self.params.pipeline_id,
            generation=self.generation(),
            healthy_actors=?healthy_actors,
            failed_or_unhealthy_actors=?failure_or_unhealthy_actors,
            success_actors=?success_actors,
            "Indexing pipeline running."
        );
        Health::Healthy
    }

    fn generation(&self) -> usize {
        self.statistics.generation
    }

    // TODO this should return an error saying whether we can retry or not.
    #[instrument(
        name="spawn_pipeline",
        level="info",
        skip_all,
        fields(
            index=%self.params.pipeline_id.index_id,
            gen=self.generation()
        ))]
    async fn spawn_pipeline(&mut self, ctx: &ActorContext<Self>) -> anyhow::Result<()> {
        let _spawn_pipeline_permit = SPAWN_PIPELINE_SEMAPHORE.acquire().await.expect("Failed to acquire spawn pipeline permit. This should never happen! Please, report on https://github.com/quickwit-oss/quickwit/issues.");
        self.statistics.num_spawn_attempts += 1;
        self.kill_switch = ctx.kill_switch().child();
        info!(
            index_id=%self.params.pipeline_id.index_id,
            source_id=%self.params.pipeline_id.source_id,
            pipeline_ord=%self.params.pipeline_id.pipeline_ord,
            root_dir=%self.params.indexing_directory.path().display(),
            "Spawning indexing pipeline.",
        );
        let (source_mailbox, source_inbox) =
            create_mailbox::<SourceActor>("SourceActor".to_string(), QueueCapacity::Unbounded);

        // Publisher
        let publisher = Publisher::new(
            PublisherType::MainPublisher,
            self.params.metastore.clone(),
            Some(self.params.merge_planner_mailbox.clone()),
            Some(source_mailbox.clone()),
        );
        let (publisher_mailbox, publisher_handler) = ctx
            .spawn_actor()
            .set_kill_switch(self.kill_switch.clone())
            .spawn(publisher);

        let sequencer = Sequencer::new(publisher_mailbox);
        let (sequencer_mailbox, sequencer_handler) = ctx
            .spawn_actor()
            .set_kill_switch(self.kill_switch.clone())
            .spawn(sequencer);

        // Uploader
        let uploader = Uploader::new(
            UploaderType::IndexUploader,
            self.params.metastore.clone(),
            self.params.split_store.clone(),
            SplitsUpdateMailbox::Sequencer(sequencer_mailbox),
            self.params.max_concurrent_split_uploads_index,
        );
        let (uploader_mailbox, uploader_handler) = ctx
            .spawn_actor()
            .set_kill_switch(self.kill_switch.clone())
            .spawn(uploader);

        // Packager
        let tag_fields = self.params.doc_mapper.tag_named_fields()?;
        let packager = Packager::new("Packager", tag_fields, uploader_mailbox);
        let (packager_mailbox, packager_handler) = ctx
            .spawn_actor()
            .set_kill_switch(self.kill_switch.clone())
            .spawn(packager);

        // Index Serializer
        let index_serializer = IndexSerializer::new(packager_mailbox);
        let (index_serializer_mailbox, index_serializer_handler) = ctx
            .spawn_actor()
            .set_kill_switch(self.kill_switch.clone())
            .spawn(index_serializer);

        // Indexer
        let indexer = Indexer::new(
            self.params.pipeline_id.clone(),
            self.params.doc_mapper.clone(),
            self.params.metastore.clone(),
            self.params.indexing_directory.clone(),
            self.params.indexing_settings.clone(),
            index_serializer_mailbox,
        );
        let (indexer_mailbox, indexer_handler) = ctx
            .spawn_actor()
            .set_kill_switch(self.kill_switch.clone())
            .spawn(indexer);

        let doc_processor = DocProcessor::new(
            self.params.pipeline_id.index_id.clone(),
            self.params.pipeline_id.source_id.clone(),
            self.params.doc_mapper.clone(),
            indexer_mailbox,
        );
        let (doc_processor_mailbox, doc_processor_handler) = ctx
            .spawn_actor()
            .set_kill_switch(self.kill_switch.clone())
            .spawn(doc_processor);

        // Fetch index_metadata to be sure to have the last updated checkpoint.
        let index_metadata = self
            .params
            .metastore
            .index_metadata(&self.params.pipeline_id.index_id)
            .await?;
        let source_checkpoint = index_metadata
            .checkpoint
            .source_checkpoint(&self.params.pipeline_id.source_id)
            .cloned()
            .unwrap_or_default(); // TODO Have a stricter check.
        let source = quickwit_supported_sources()
            .load_source(
                Arc::new(SourceExecutionContext {
                    metastore: self.params.metastore.clone(),
                    index_id: self.params.pipeline_id.index_id.clone(),
                    queues_dir_path: self.params.queues_dir_path.clone(),
                    source_config: self.params.source_config.clone(),
                }),
                source_checkpoint,
            )
            .await?;
        let actor_source = SourceActor {
            source,
            doc_processor_mailbox,
        };
        let (_source_mailbox, source_handler) = ctx
            .spawn_actor()
            .set_mailboxes(source_mailbox, source_inbox)
            .set_kill_switch(self.kill_switch.clone())
            .spawn(actor_source);

        // Increment generation once we are sure there will be no spawning error.
        self.previous_generations_statistics = self.statistics.clone();
        self.statistics.generation += 1;
        self.handles = Some(IndexingPipelineHandles {
            source: source_handler,
            doc_processor: doc_processor_handler,
            indexer: indexer_handler,
            index_serializer: index_serializer_handler,
            packager: packager_handler,
            uploader: uploader_handler,
            sequencer: sequencer_handler,
            publisher: publisher_handler,
        });
        Ok(())
    }

    async fn terminate(&mut self) {
        self.kill_switch.kill();
        if let Some(handlers) = self.handles.take() {
            tokio::join!(
                handlers.source.kill(),
                handlers.indexer.kill(),
                handlers.packager.kill(),
                handlers.uploader.kill(),
                handlers.publisher.kill(),
            );
        }
    }
}

#[async_trait]
impl Handler<Observe> for IndexingPipeline {
    type Reply = ();
    async fn handle(
        &mut self,
        _: Observe,
        ctx: &ActorContext<Self>,
    ) -> Result<(), ActorExitStatus> {
        if let Some(handles) = &self.handles {
            let (doc_processor_counters, indexer_counters, uploader_counters, publisher_counters) = join!(
                handles.doc_processor.observe(),
                handles.indexer.observe(),
                handles.uploader.observe(),
                handles.publisher.observe(),
            );
            self.statistics = self
                .previous_generations_statistics
                .clone()
                .add_actor_counters(
                    &*doc_processor_counters,
                    &*indexer_counters,
                    &*uploader_counters,
                    &*publisher_counters,
                )
                .set_generation(self.statistics.generation)
                .set_num_spawn_attempts(self.statistics.num_spawn_attempts);
        }
        ctx.schedule_self_msg(Duration::from_secs(1), Observe).await;
        Ok(())
    }
}

#[async_trait]
impl Handler<Supervise> for IndexingPipeline {
    type Reply = ();

    async fn handle(
        &mut self,
        _: Supervise,
        ctx: &ActorContext<Self>,
    ) -> Result<(), ActorExitStatus> {
        if self.handles.is_some() {
            match self.healthcheck() {
                Health::Healthy => {}
                Health::FailureOrUnhealthy => {
                    self.terminate().await;
                    ctx.schedule_self_msg(quickwit_actors::HEARTBEAT, Spawn { retry_count: 0 })
                        .await;
                }
                Health::Success => {
                    return Err(ActorExitStatus::Success);
                }
            }
        }
        ctx.schedule_self_msg(quickwit_actors::HEARTBEAT, Supervise)
            .await;
        Ok(())
    }
}

#[async_trait]
impl Handler<Spawn> for IndexingPipeline {
    type Reply = ();

    async fn handle(
        &mut self,
        spawn: Spawn,
        ctx: &ActorContext<Self>,
    ) -> Result<(), ActorExitStatus> {
        if self.handles.is_some() {
            return Ok(());
        }
        self.previous_generations_statistics.num_spawn_attempts = 1 + spawn.retry_count;
        if let Err(spawn_error) = self.spawn_pipeline(ctx).await {
            if let Some(MetastoreError::IndexDoesNotExist { .. }) =
                spawn_error.downcast_ref::<MetastoreError>()
            {
                info!(error = ?spawn_error, "Could not spawn pipeline, index might have been deleted.");
                return Err(ActorExitStatus::Success);
            }
            let retry_delay = wait_duration_before_retry(spawn.retry_count);
            error!(error = ?spawn_error, retry_count = spawn.retry_count, retry_delay = ?retry_delay, "Error while spawning indexing pipeline, retrying after some time.");
            ctx.schedule_self_msg(
                retry_delay,
                Spawn {
                    retry_count: spawn.retry_count + 1,
                },
            )
            .await;
        }
        Ok(())
    }
}

pub struct IndexingPipelineParams {
    pub pipeline_id: IndexingPipelineId,
    pub doc_mapper: Arc<dyn DocMapper>,
    pub indexing_directory: IndexingDirectory,
    pub queues_dir_path: PathBuf,
    pub indexing_settings: IndexingSettings,
    pub source_config: SourceConfig,
    pub metastore: Arc<dyn Metastore>,
    pub storage: Arc<dyn Storage>,
    pub split_store: IndexingSplitStore,
    pub max_concurrent_split_uploads_index: usize,
    pub max_concurrent_split_uploads_merge: usize,
    pub merge_planner_mailbox: Mailbox<MergePlanner>,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use quickwit_actors::Universe;
    use quickwit_config::{IndexingSettings, SourceParams};
    use quickwit_doc_mapper::default_doc_mapper_for_test;
    use quickwit_metastore::{IndexMetadata, MetastoreError, MockMetastore};
    use quickwit_storage::RamStorage;

    use super::{IndexingPipeline, *};
    use crate::models::IndexingDirectory;

    #[test]
    fn test_wait_duration() {
        assert_eq!(wait_duration_before_retry(0), Duration::from_secs(2));
        assert_eq!(wait_duration_before_retry(1), Duration::from_secs(4));
        assert_eq!(wait_duration_before_retry(2), Duration::from_secs(8));
        assert_eq!(wait_duration_before_retry(3), Duration::from_secs(16));
        assert_eq!(wait_duration_before_retry(8), Duration::from_secs(512));
        assert_eq!(wait_duration_before_retry(9), MAX_RETRY_DELAY);
    }

    async fn test_indexing_pipeline_num_fails_before_success(
        mut num_fails: usize,
    ) -> anyhow::Result<bool> {
        let mut metastore = MockMetastore::default();
        metastore
            .expect_index_metadata()
            .withf(|index_id| index_id == "test-index")
            .returning(move |_| {
                if num_fails == 0 {
                    let index_metadata =
                        IndexMetadata::for_test("test-index", "ram:///indexes/test-index");
                    return Ok(index_metadata);
                }
                num_fails -= 1;
                Err(MetastoreError::ConnectionError {
                    message: "MetastoreError Alarm".to_string(),
                })
            });
        metastore
            .expect_last_delete_opstamp()
            .returning(move |index_id| {
                assert_eq!("test-index", index_id);
                Ok(10)
            });
        metastore
            .expect_mark_splits_for_deletion()
            .returning(|_, _| Ok(()));
        metastore
            .expect_stage_split()
            .withf(|index_id, _metadata| -> bool { index_id == "test-index" })
            .times(1)
            .returning(|_, _| Ok(()));
        metastore
            .expect_publish_splits()
            .withf(
                |index_id, splits, replaced_splits, checkpoint_delta_opt| -> bool {
                    let checkpoint_delta = checkpoint_delta_opt.as_ref().unwrap();
                    index_id == "test-index"
                        && checkpoint_delta.source_id == "test-source"
                        && splits.len() == 1
                        && replaced_splits.is_empty()
                        && format!("{:?}", checkpoint_delta.source_delta)
                            .ends_with(":(00000000000000000000..00000000000000001030])")
                },
            )
            .times(1)
            .returning(|_, _, _, _| Ok(()));
        let universe = Universe::new();
        let node_id = "test-node";
        let metastore = Arc::new(metastore);
        let pipeline_id = IndexingPipelineId {
            index_id: "test-index".to_string(),
            source_id: "test-source".to_string(),
            node_id: node_id.to_string(),
            pipeline_ord: 0,
        };
        let source_config = SourceConfig {
            source_id: "test-source".to_string(),
            num_pipelines: 1,
            enabled: true,
            source_params: SourceParams::file(PathBuf::from("data/test_corpus.json")),
        };
        let storage = Arc::new(RamStorage::default());
        let split_store = IndexingSplitStore::create_without_local_store(storage.clone());
        let (merge_planner_mailbox, _) =
            create_mailbox("MergePlanner".to_string(), QueueCapacity::Unbounded);
        let pipeline_params = IndexingPipelineParams {
            pipeline_id,
            doc_mapper: Arc::new(default_doc_mapper_for_test()),
            source_config,
            indexing_directory: IndexingDirectory::for_test().await,
            indexing_settings: IndexingSettings::for_test(),
            metastore: metastore.clone(),
            storage,
            split_store,
            queues_dir_path: PathBuf::from("./queues"),
            max_concurrent_split_uploads_index: 4,
            max_concurrent_split_uploads_merge: 5,
            merge_planner_mailbox,
        };
        let pipeline = IndexingPipeline::new(pipeline_params);
        let (_pipeline_mailbox, pipeline_handler) = universe.spawn_builder().spawn(pipeline);
        let (pipeline_exit_status, pipeline_statistics) = pipeline_handler.join().await;
        assert_eq!(pipeline_statistics.generation, 1);
        assert_eq!(pipeline_statistics.num_spawn_attempts, 1 + num_fails);
        Ok(pipeline_exit_status.is_success())
    }

    #[tokio::test]
    async fn test_indexing_pipeline_retry_0() -> anyhow::Result<()> {
        test_indexing_pipeline_num_fails_before_success(0).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_indexing_pipeline_retry_1() -> anyhow::Result<()> {
        test_indexing_pipeline_num_fails_before_success(1).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_indexing_pipeline_simple() -> anyhow::Result<()> {
        let mut metastore = MockMetastore::default();
        metastore
            .expect_index_metadata()
            .withf(|index_id| index_id == "test-index")
            .returning(|_| {
                Ok(IndexMetadata::for_test(
                    "test-index",
                    "ram:///indexes/test-index",
                ))
            });
        metastore
            .expect_last_delete_opstamp()
            .returning(move |index_id| {
                assert_eq!("test-index", index_id);
                Ok(10)
            });
        metastore
            .expect_stage_split()
            .withf(|index_id, _metadata| index_id == "test-index")
            .times(1)
            .returning(|_, _| Ok(()));
        metastore
            .expect_publish_splits()
            .withf(
                |index_id, splits, replaced_split_ids, checkpoint_delta_opt| -> bool {
                    let checkpoint_delta = checkpoint_delta_opt.as_ref().unwrap();
                    index_id == "test-index"
                        && splits.len() == 1
                        && replaced_split_ids.is_empty()
                        && checkpoint_delta.source_id == "test-source"
                        && format!("{:?}", checkpoint_delta.source_delta)
                            .ends_with(":(00000000000000000000..00000000000000001030])")
                },
            )
            .times(1)
            .returning(|_, _, _, _| Ok(()));
        let universe = Universe::new();
        let node_id = "test-node";
        let metastore = Arc::new(metastore);
        let pipeline_id = IndexingPipelineId {
            index_id: "test-index".to_string(),
            source_id: "test-source".to_string(),
            node_id: node_id.to_string(),
            pipeline_ord: 0,
        };
        let source_config = SourceConfig {
            source_id: "test-source".to_string(),
            num_pipelines: 1,
            enabled: true,
            source_params: SourceParams::file(PathBuf::from("data/test_corpus.json")),
        };
        let storage = Arc::new(RamStorage::default());
        let split_store = IndexingSplitStore::create_without_local_store(storage.clone());
        let (merge_planner_mailbox, _) =
            create_mailbox("MergePlanner".to_string(), QueueCapacity::Unbounded);
        let pipeline_params = IndexingPipelineParams {
            pipeline_id,
            doc_mapper: Arc::new(default_doc_mapper_for_test()),
            source_config,
            indexing_directory: IndexingDirectory::for_test().await,
            indexing_settings: IndexingSettings::for_test(),
            metastore: metastore.clone(),
            queues_dir_path: PathBuf::from("./queues"),
            storage,
            split_store,
            max_concurrent_split_uploads_index: 4,
            max_concurrent_split_uploads_merge: 5,
            merge_planner_mailbox,
        };
        let pipeline = IndexingPipeline::new(pipeline_params);
        let (_pipeline_mailbox, pipeline_handler) = universe.spawn_builder().spawn(pipeline);
        let (pipeline_exit_status, pipeline_statistics) = pipeline_handler.join().await;
        assert!(pipeline_exit_status.is_success());
        assert_eq!(pipeline_statistics.generation, 1);
        assert_eq!(pipeline_statistics.num_spawn_attempts, 1);
        assert_eq!(pipeline_statistics.num_published_splits, 1);
        Ok(())
    }
}
