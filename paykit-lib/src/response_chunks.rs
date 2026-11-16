//! THROWAWAY (#411): portable chunked reads of a `reqwest::Response`.
//!
//! Native reqwest exposes `Response::chunk()`, but the wasm32 (browser `fetch`)
//! backend does not. Every bounded body read in Paykit pulls one chunk at a
//! time so it can stop as soon as a byte limit is crossed, which matters because
//! homeserver bodies are untrusted. This wrapper keeps that property on both
//! targets: native calls `chunk()`, wasm32 drives `bytes_stream()`.
//!
//! Security note: on wasm32 the browser's `fetch` streaming decides the chunk
//! size, so a single chunk can be larger than the remaining budget. Callers
//! still reject before buffering it, but the browser itself may already have
//! buffered that chunk. Native has the same property with hyper's frames.

/// One chunk of response bytes.
pub type ResponseChunk = bytes::Bytes;

/// Incremental reader over a response body.
#[doc(hidden)]
pub struct ResponseChunks {
    #[cfg(not(target_arch = "wasm32"))]
    response: reqwest::Response,
    #[cfg(target_arch = "wasm32")]
    stream: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = reqwest::Result<ResponseChunk>> + 'static>,
    >,
}

impl ResponseChunks {
    /// Take ownership of a response so its body can be read chunk by chunk.
    pub fn new(response: reqwest::Response) -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            Self { response }
        }
        #[cfg(target_arch = "wasm32")]
        {
            Self {
                stream: Box::pin(response.bytes_stream()),
            }
        }
    }

    /// Return the next chunk, or `None` at the end of the body.
    pub async fn next_chunk(&mut self) -> reqwest::Result<Option<ResponseChunk>> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.response.chunk().await
        }
        #[cfg(target_arch = "wasm32")]
        {
            futures_util::StreamExt::next(&mut self.stream)
                .await
                .transpose()
        }
    }
}
