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

mod indexing_pipeline;
mod merge_pipeline;

mod doc_processor;
mod index_serializer;
mod indexer;
mod indexing_service;
mod ingest_api_garbage_collector;
mod packager;
mod publisher;
mod sequencer;
mod uploader;

pub use indexing_pipeline::{IndexingPipeline, IndexingPipelineHandles, IndexingPipelineParams};
pub use indexing_service::{
    IndexingService, IndexingServiceError, IndexingServiceState, INDEXING_DIR_NAME,
};
pub use sequencer::Sequencer;
mod merge_executor;
mod merge_planner;
mod merge_split_downloader;

pub use self::doc_processor::{DocProcessor, DocProcessorCounters};
pub use self::index_serializer::IndexSerializer;
pub use self::indexer::{Indexer, IndexerCounters};
pub use self::ingest_api_garbage_collector::{
    IngestApiGarbageCollector, IngestApiGarbageCollectorCounters,
};
pub use self::merge_executor::{combine_partition_ids, merge_split_attrs, MergeExecutor};
pub use self::merge_planner::MergePlanner;
pub use self::merge_split_downloader::MergeSplitDownloader;
pub use self::packager::Packager;
pub use self::publisher::{Publisher, PublisherCounters, PublisherType};
pub use self::uploader::{SplitsUpdateMailbox, Uploader, UploaderCounters, UploaderType};
