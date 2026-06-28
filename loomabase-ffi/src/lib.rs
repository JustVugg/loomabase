//! C ABI bindings to the Loomabase CRDT protocol core.
//!
//! This crate is the foundation for language SDKs (Swift, Kotlin, C, Python via
//! cffi, …). It exposes the in-memory reference merge over a small, stable C
//! ABI: callers JSON-encode a [`loomabase::crdt::SyncPayload`], merge it for a
//! device, and receive the JSON server response. Storage and transport stay on
//! the host side.
//!
//! It lives in a separate crate because the C ABI requires `unsafe`, which the
//! core `loomabase` crate forbids.

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;

use loomabase::crdt::{CrdtState, SyncPayload};
use loomabase::schema::todos_table;

pub const LOOMABASE_ABI_VERSION: u32 = 1;

/// Opaque, thread-safe handle owned by C callers.
pub struct LoomabaseState {
    inner: Mutex<CrdtState>,
}

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_last_error(message: impl Into<String>) {
    let message = message.into().replace('\0', " ");
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = CString::new(message).ok();
    });
}

fn clear_last_error() {
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = None;
    });
}

/// Returns the ABI version implemented by this library.
#[unsafe(no_mangle)]
pub extern "C" fn loomabase_abi_version() -> u32 {
    LOOMABASE_ABI_VERSION
}

/// Returns the latest error message for the calling thread, or null. The
/// borrowed pointer remains valid until that thread calls another Loomabase
/// function.
#[unsafe(no_mangle)]
pub extern "C" fn loomabase_last_error_message() -> *const c_char {
    LAST_ERROR.with(|slot| {
        slot.borrow()
            .as_ref()
            .map_or(std::ptr::null(), |message| message.as_ptr())
    })
}

/// Creates a new in-memory reference server for the canonical `todos` contract.
/// Free it with [`loomabase_state_free`].
#[unsafe(no_mangle)]
pub extern "C" fn loomabase_state_new() -> *mut LoomabaseState {
    clear_last_error();
    Box::into_raw(Box::new(LoomabaseState {
        inner: Mutex::new(CrdtState::new(todos_table())),
    }))
}

/// Merges a JSON-encoded `SyncPayload` for `device_id` and returns a freshly
/// allocated JSON server-response string, or null on any error (invalid
/// pointers, malformed JSON, or a rejected merge). Free the result with
/// [`loomabase_string_free`].
///
/// # Safety
/// `state` must come from [`loomabase_state_new`] and not be freed; both
/// `payload_json` and `device_id` must be valid NUL-terminated UTF-8 C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn loomabase_state_merge(
    state: *mut LoomabaseState,
    payload_json: *const c_char,
    device_id: *const c_char,
) -> *mut c_char {
    clear_last_error();
    if let Ok(result) = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: upheld by this function's caller contract.
        unsafe { merge_inner(state, payload_json, device_id) }
    })) {
        result
    } else {
        set_last_error("unexpected panic while merging");
        std::ptr::null_mut()
    }
}

unsafe fn merge_inner(
    state: *mut LoomabaseState,
    payload_json: *const c_char,
    device_id: *const c_char,
) -> *mut c_char {
    if payload_json.is_null() || device_id.is_null() {
        set_last_error("payload_json and device_id must not be null");
        return std::ptr::null_mut();
    }
    let Some(state) = (unsafe { state.as_mut() }) else {
        set_last_error("state must not be null");
        return std::ptr::null_mut();
    };
    let Ok(payload_str) = (unsafe { CStr::from_ptr(payload_json) }).to_str() else {
        set_last_error("payload_json must be valid UTF-8");
        return std::ptr::null_mut();
    };
    let Ok(device) = (unsafe { CStr::from_ptr(device_id) }).to_str() else {
        set_last_error("device_id must be valid UTF-8");
        return std::ptr::null_mut();
    };
    let payload = match serde_json::from_str::<SyncPayload>(payload_str) {
        Ok(payload) => payload,
        Err(error) => {
            set_last_error(format!("invalid payload JSON: {error}"));
            return std::ptr::null_mut();
        }
    };
    let response = if let Ok(mut inner) = state.inner.lock() {
        match inner.merge(payload, device) {
            Ok(response) => response,
            Err(error) => {
                set_last_error(error.to_string());
                return std::ptr::null_mut();
            }
        }
    } else {
        set_last_error("state lock is poisoned");
        return std::ptr::null_mut();
    };
    match serde_json::to_string(&response).map(CString::new) {
        Ok(Ok(json)) => json.into_raw(),
        Ok(Err(_)) => {
            set_last_error("response contains an interior NUL byte");
            std::ptr::null_mut()
        }
        Err(error) => {
            set_last_error(format!("could not serialize response: {error}"));
            std::ptr::null_mut()
        }
    }
}

/// Frees a reference server created by [`loomabase_state_new`].
///
/// # Safety
/// `state` must come from [`loomabase_state_new`] and not be freed twice.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn loomabase_state_free(state: *mut LoomabaseState) {
    clear_last_error();
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state) });
    }
}

/// Frees a string returned by [`loomabase_state_merge`].
///
/// # Safety
/// `string` must come from this library and not be freed twice.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn loomabase_string_free(string: *mut c_char) {
    clear_last_error();
    if !string.is_null() {
        drop(unsafe { CString::from_raw(string) });
    }
}
