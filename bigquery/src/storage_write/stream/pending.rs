use crate::grpc::apiv1::bigquery_client::create_write_stream_request;
use crate::grpc::apiv1::conn_pool::ConnectionManager;
use crate::storage_write::stream::{AsStream, DisposableStream, ManagedStream, Stream};
use google_cloud_gax::grpc::Status;
use google_cloud_googleapis::cloud::bigquery::storage::v1::write_stream::Type::Pending;
use google_cloud_googleapis::cloud::bigquery::storage::v1::{
    BatchCommitWriteStreamsRequest, BatchCommitWriteStreamsResponse,
};
use std::sync::Arc;

pub struct Writer {
    max_insert_count: usize,
    cm: Arc<ConnectionManager>,
    table: String,
    streams: Vec<String>,
}

impl Writer {
    pub(crate) fn new(max_insert_count: usize, cm: Arc<ConnectionManager>, table: String) -> Self {
        Self {
            max_insert_count,
            cm,
            table,
            streams: Vec::new(),
        }
    }

    pub async fn create_write_stream(&mut self) -> Result<PendingStream, Status> {
        let req = create_write_stream_request(&self.table, Pending);
        let stream = self.cm.writer().create_write_stream(req, None).await?.into_inner();
        self.streams.push(stream.name.to_string());
        Ok(PendingStream::new(Stream::new(stream, self.cm.clone(), self.max_insert_count)))
    }

    pub async fn commit(&self) -> Result<BatchCommitWriteStreamsResponse, Status> {
        let result = self
            .cm
            .writer()
            .batch_commit_write_streams(
                BatchCommitWriteStreamsRequest {
                    parent: self.table.to_string(),
                    write_streams: self.streams.clone(),
                },
                None,
            )
            .await?
            .into_inner();
        Ok(result)
    }
}
pub struct PendingStream {
    inner: Stream,
}

impl PendingStream {
    pub(crate) fn new(inner: Stream) -> Self {
        Self { inner }
    }
}

impl AsStream for PendingStream {
    fn as_ref(&self) -> &Stream {
        &self.inner
    }
}
impl ManagedStream for PendingStream {}
impl DisposableStream for PendingStream {}

#[cfg(test)]
mod tests {
    use crate::client::{Client, ClientConfig};
    use crate::storage_write::stream::tests::{create_append_rows_request, TestData};
    use crate::storage_write::stream::{DisposableStream, ManagedStream};
    use futures_util::StreamExt;
    use google_cloud_gax::grpc::Status;
    use prost::Message;
    use std::sync::Arc;
    use tokio::task::JoinHandle;

    #[ctor::ctor]
    fn init() {
        crate::storage_write::stream::tests::init();
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn test_storage_write() {
        let (config, project_id) = ClientConfig::new_with_auth().await.unwrap();
        let project_id = project_id.unwrap();
        let client = Client::new(config).await.unwrap();
        let tables = ["write_test", "write_test_1"];

        // Create Writers
        let mut writers = vec![];
        for i in 0..2 {
            let table = format!(
                "projects/{}/datasets/gcrbq_storage/tables/{}",
                &project_id,
                tables[i % tables.len()]
            );
            let writer = client.pending_storage_writer(&table);
            writers.push(writer);
        }

        // Create Streams
        let mut streams = vec![];
        for writer in writers.iter_mut() {
            let stream = writer.create_write_stream().await.unwrap();
            streams.push(stream);
        }

        // Append Rows
        let mut tasks: Vec<JoinHandle<Result<(), Status>>> = vec![];
        for (i, stream) in streams.into_iter().enumerate() {
            tasks.push(tokio::spawn(async move {
                let mut rows = vec![];
                for j in 0..5 {
                    let data = TestData {
                        col_string: format!("pending_{i}_{j}"),
                    };
                    let mut buf = Vec::new();
                    data.encode(&mut buf).unwrap();
                    rows.push(create_append_rows_request(vec![buf.clone(), buf.clone(), buf]));
                }
                let mut result = stream.append_rows(rows).await.unwrap();
                while let Some(res) = result.next().await {
                    let res = res?;
                    tracing::info!("append row errors = {:?}", res.row_errors.len());
                }
                let result = stream.finalize().await.unwrap();
                tracing::info!("finalized row count = {:?}", result);
                Ok(())
            }));
        }

        // Wait for append rows
        for task in tasks {
            task.await.unwrap().unwrap();
        }

        for writer in writers.iter_mut() {
            let result = writer.commit().await.unwrap();
            tracing::info!("committed error count = {:?}", result.stream_errors.len());
        }
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn test_storage_write_single_stream() {
        let (config, project_id) = ClientConfig::new_with_auth().await.unwrap();
        let project_id = project_id.unwrap();
        let client = Client::new(config).await.unwrap();

        // Create Streams
        let mut streams = vec![];
        let table = format!("projects/{}/datasets/gcrbq_storage/tables/write_test", &project_id);
        let mut writer = client.pending_storage_writer(&table);
        let stream = Arc::new(writer.create_write_stream().await.unwrap());
        for i in 0..2 {
            streams.push(stream.clone());
        }

        // Append Rows
        let mut tasks: Vec<JoinHandle<Result<(), Status>>> = vec![];
        for (i, stream) in streams.into_iter().enumerate() {
            tasks.push(tokio::spawn(async move {
                let mut rows = vec![];
                for j in 0..5 {
                    let data = TestData {
                        col_string: format!("pending_{i}_{j}"),
                    };
                    let mut buf = Vec::new();
                    data.encode(&mut buf).unwrap();
                    rows.push(create_append_rows_request(vec![buf.clone(), buf.clone(), buf]));
                }
                let mut result = stream.append_rows(rows).await.unwrap();
                while let Some(res) = result.next().await {
                    let res = res?;
                    tracing::info!("append row errors = {:?}", res.row_errors.len());
                }
                Ok(())
            }));
        }

        // Wait for append rows
        for task in tasks {
            task.await.unwrap().unwrap();
        }

        let result = stream.finalize().await.unwrap();
        tracing::info!("finalized row count = {:?}", result);

        let result = writer.commit().await.unwrap();
        tracing::info!("commit error count = {:?}", result.stream_errors.len());
    }
}