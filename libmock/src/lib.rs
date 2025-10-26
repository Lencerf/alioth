use libc::{RTLD_NEXT, c_char, dlsym};
use std::ffi::{CStr, c_void};

mod dylib;

pub fn add_mocks(name: &'static CStr, f: *mut c_void) {
    let o_f = unsafe { dlsym(RTLD_NEXT, c"add_mock".as_ptr()) };
    assert!(!o_f.is_null());
    let f_add_mock = unsafe {
        std::mem::transmute::<*mut c_void, extern "C" fn(*const c_char, *mut c_void)>(o_f)
    };
    f_add_mock(name.as_ptr(), f)
}
