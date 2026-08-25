#![doc = "UniFFI bindings for Paykit SDK."]

mod config;
mod conversions_common;
mod errors;
mod json;
mod payment_adapter;
mod payment_requests;
mod payment_resolution;
mod private_links;
mod private_lists;
mod profiles;
mod receipts;
mod sdk;
mod secrets;
mod session;
mod storage;

#[cfg(test)]
mod tests;

pub use config::*;
pub use errors::PaykitFfiError;
pub use json::*;
pub use payment_adapter::*;
pub use payment_requests::*;
pub use payment_resolution::*;
pub use private_links::*;
pub use private_lists::*;
pub use profiles::*;
pub use receipts::*;
pub use sdk::*;
pub use secrets::*;
pub use session::*;
pub use storage::*;

uniffi::setup_scaffolding!();

// The FFI blob envelope versions alias the SDK generation constants so the
// envelopes move in lockstep with SDK storage/backup semantics by
// construction rather than by convention.
pub(crate) const SDK_STATE_BLOB_VERSION: u32 = paykit_sdk::storage::SDK_STORAGE_STATE_GENERATION;
pub(crate) const SDK_STATE_BLOB_MIN_READ_VERSION: u32 =
    paykit_sdk::storage::SDK_STORAGE_STATE_MIN_READ_GENERATION;
pub(crate) const SDK_BACKUP_BLOB_VERSION: u32 = paykit_sdk::SDK_BACKUP_VERSION;
pub(crate) const SDK_BACKUP_BLOB_MIN_READ_VERSION: u32 = paykit_sdk::SDK_BACKUP_MIN_READ_VERSION;
pub(crate) const DEFAULT_PUBKY_REQUEST_TIMEOUT_SECS: u64 = 30;

#[cfg(target_os = "android")]
fn init_android_logger() {
    let _ = android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("paykit"),
    );
}

#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_synonym_paykit_PaykitAndroid_nativeInitialize(
    mut env: jni::JNIEnv<'_>,
    _class: jni::objects::JClass<'_>,
    context: jni::objects::JObject<'_>,
) -> jni::sys::jboolean {
    init_android_logger();
    match rustls_platform_verifier::android::init_with_env(&mut env, context) {
        Ok(()) => jni::sys::JNI_TRUE,
        Err(err) => {
            log::error!("failed to initialize Android rustls platform verifier: {err:?}");
            jni::sys::JNI_FALSE
        }
    }
}
