// THROWAWAY (#411): browser-only async storage callbacks.
#[cfg(feature = "web")]
mod async_storage;
mod config_and_auth;
mod identity;
mod storage;
