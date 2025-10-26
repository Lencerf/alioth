use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, c_void};

use libc::{RTLD_NEXT, c_char, c_int, dlsym, mode_t};

thread_local! {
    pub static MOCKS: RefCell<HashMap<&'static CStr, VecDeque<*mut c_void>>> = RefCell::new(HashMap::new());
}

#[unsafe(no_mangle)]
pub fn add_mock(name: *const c_char, f: *mut c_void) {
    let func_name: &'static CStr = unsafe { CStr::from_ptr(name) };
    println!("Adding mock for {}", func_name.to_string_lossy());
    MOCKS.with(|mocks| {
        let mut mocks = mocks.borrow_mut();
        mocks.entry(func_name).or_default().push_back(f);
    });
}

#[unsafe(no_mangle)]
pub fn open64(path: *const c_char, oflag: c_int, mode: mode_t) -> c_int {
    let r = MOCKS.with(|mocks| {
        let mut mocks = mocks.borrow_mut();
        let Some(a) = mocks.get_mut(c"open64") else {
            return None;
        };
        let Some(f) = a.pop_front() else {
            return None;
        };
        let f = unsafe {
            std::mem::transmute::<
                *mut c_void,
                extern "C" fn(*mut bool, *const c_char, c_int, mode_t) -> c_int,
            >(f)
        };
        let mut should_skip = false;
        let ret = f(&mut should_skip, path, oflag, mode);
        if should_skip { None } else { Some(ret) }
    });
    if let Some(r) = r {
        return r;
    }
    let o_f = unsafe { dlsym(RTLD_NEXT, c"open64".as_ptr()) };
    let f = unsafe {
        std::mem::transmute::<*mut c_void, extern "C" fn(*const c_char, c_int, mode_t) -> c_int>(
            o_f,
        )
    };
    f(path, oflag, mode)
}
