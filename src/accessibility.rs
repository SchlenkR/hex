use std::ffi::{c_char, c_void};
use std::ptr;

use color_eyre::eyre::{Result, eyre};

type CfTypeRef = *const c_void;
type CfStringRef = *const c_void;
type AxUiElementRef = *const c_void;

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> AxUiElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AxUiElementRef,
        attribute: CfStringRef,
        value: *mut CfTypeRef,
    ) -> i32;
    fn AXUIElementSetMessagingTimeout(element: AxUiElementRef, timeout_in_seconds: f32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFGetTypeID(value: CfTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFStringGetLength(value: CfStringRef) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(
        value: CfStringRef,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> bool;
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        string: *const c_char,
        encoding: u32,
    ) -> CfStringRef;
    fn CFRelease(value: CfTypeRef);
}

pub fn focused_window_title(pid: i32) -> Option<String> {
    capture_focused_window_title(pid)
        .ok()
        .filter(|title| !title.is_empty())
}

fn capture_focused_window_title(pid: i32) -> Result<String> {
    // SAFETY: The create/copy APIs return retained Core Foundation objects.
    let application = unsafe { AXUIElementCreateApplication(pid) };
    if application.is_null() {
        return Err(eyre!("could not inspect the foreground application"));
    }
    if unsafe { AXUIElementSetMessagingTimeout(application, 0.25) } != 0 {
        unsafe { CFRelease(application) };
        return Err(eyre!(
            "could not bound communication with the foreground application"
        ));
    }

    let focused_window_attribute = cf_string_literal(c"AXFocusedWindow")?;
    let mut window = ptr::null();
    let window_status = unsafe {
        AXUIElementCopyAttributeValue(application, focused_window_attribute, &mut window)
    };
    unsafe { CFRelease(focused_window_attribute) };
    unsafe { CFRelease(application) };
    if window_status != 0 || window.is_null() {
        return Err(eyre!("the foreground application has no focused window"));
    }
    if unsafe { AXUIElementSetMessagingTimeout(window.cast(), 0.25) } != 0 {
        unsafe { CFRelease(window) };
        return Err(eyre!(
            "could not bound communication with the foreground window"
        ));
    }

    let title_attribute = cf_string_literal(c"AXTitle")?;
    let mut title = ptr::null();
    let title_status =
        unsafe { AXUIElementCopyAttributeValue(window.cast(), title_attribute, &mut title) };
    unsafe { CFRelease(title_attribute) };
    unsafe { CFRelease(window) };
    if title_status != 0 || title.is_null() {
        return Err(eyre!("the foreground window does not expose a title"));
    }
    let result = cf_string(title);
    unsafe { CFRelease(title) };
    result
}

fn cf_string_literal(value: &std::ffi::CStr) -> Result<CfStringRef> {
    let string =
        unsafe { CFStringCreateWithCString(ptr::null(), value.as_ptr(), CF_STRING_ENCODING_UTF8) };
    (!string.is_null())
        .then_some(string)
        .ok_or_else(|| eyre!("could not create an Accessibility attribute name"))
}

fn cf_string(value: CfTypeRef) -> Result<String> {
    if unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() } {
        return Err(eyre!("the window title has an unsupported value type"));
    }
    let value = value.cast();
    let length = unsafe { CFStringGetLength(value) };
    let capacity = unsafe { CFStringGetMaximumSizeForEncoding(length, CF_STRING_ENCODING_UTF8) }
        .saturating_add(1);
    let mut bytes = vec![0_u8; capacity as usize];
    if !unsafe {
        CFStringGetCString(
            value,
            bytes.as_mut_ptr().cast(),
            capacity,
            CF_STRING_ENCODING_UTF8,
        )
    } {
        return Err(eyre!("could not decode the window title"));
    }
    let length = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8(bytes[..length].to_vec()).map_err(Into::into)
}
