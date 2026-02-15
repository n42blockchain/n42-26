/// Network layer errors.
#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    /// Message encoding/decoding failure.
    #[error("codec error: {0}")]
    Codec(String),

    /// Failed to subscribe to a GossipSub topic.
    #[error("subscription error: {0}")]
    Subscribe(String),

    /// Failed to start listening.
    #[error("listen error: {0}")]
    Listen(String),

    /// Failed to dial a peer.
    #[error("dial error: {0}")]
    Dial(String),

    /// Command channel was closed (service shut down).
    #[error("network service channel closed")]
    ChannelClosed,
}
