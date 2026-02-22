/// Network layer errors.
#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    #[error("codec error: {0}")]
    Codec(String),

    #[error("subscription error: {0}")]
    Subscribe(String),

    #[error("listen error: {0}")]
    Listen(String),

    #[error("dial error: {0}")]
    Dial(String),

    #[error("network service channel closed")]
    ChannelClosed,
}
