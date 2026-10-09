#![allow(dead_code)]
#[path = "qmdb_read_view.rs"]
mod qmdb_read_view;
#[path = "qmdb_read_status.rs"]
mod qmdb_read_status;
mod qmdb_state_reader {
    #[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum QmdbReadsMode { Off, Verify, On, Only }
}
