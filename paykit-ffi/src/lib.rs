#![doc = "UniFFI bindings for Paykit SDK."]

macro_rules! impl_redacted_debug {
    ($($type:ident),+ $(,)?) => {
        $(
            impl std::fmt::Debug for $type {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str(concat!(stringify!($type), "(<redacted>)"))
                }
            }
        )+
    };
}

mod allowance_accounting;
mod allowances;
mod app_registry;
mod config;
mod conversions_common;
mod errors;
// THROWAWAY (#411): grant logout builds strict native reqwest clients (timeouts,
// no redirects, platform TLS verifier) that wasm reqwest cannot express, and
// paykit-sdk only exports it off wasm32. The browser API lacks it for now.
#[cfg(not(feature = "web"))]
mod grant_logout;
mod grant_observation;
mod json;
mod observation;
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

pub use allowance_accounting::*;
pub use allowances::*;
pub use app_registry::*;
pub use config::*;
pub use errors::PaykitFfiError;
#[cfg(not(feature = "web"))]
pub use grant_logout::*;
pub use grant_observation::*;
pub use json::*;
pub use observation::*;
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

// THROWAWAY (#411): the `web` cargo feature builds the browser (ubrn web flavor)
// variant of this crate. It must be a cargo feature, not a target_arch cfg,
// because ubrn 0.29 reads UniFFI metadata from a native build of the crate and
// then generates a wasm shim that calls the same exports by symbol name, so
// both builds must export the same set. Under `web`:
// - async exports drop `async_runtime = "tokio"`: async-compat would spawn a
//   std thread on first use, which panics on wasm32-unknown-unknown;
// - grant logout is not exported (see above);
// - `web_install_panic_hook` and the async storage/session callbacks exist.
// Native builds without `web` export exactly what they did before.

/// Browser build only: route Rust panics to `console.error`.
///
/// THROWAWAY (#411): without it a panic shows up only as
/// `RuntimeError: unreachable`.
#[cfg(feature = "web")]
#[uniffi::export]
pub fn web_install_panic_hook() {
    #[cfg(target_arch = "wasm32")]
    console_error_panic_hook::set_once();
}

pub(crate) const SDK_BACKUP_BLOB_VERSION: u32 = 2;
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
