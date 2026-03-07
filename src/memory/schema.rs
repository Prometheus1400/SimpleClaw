use std::sync::OnceLock;

use crate::error::FrameworkError;

pub(super) fn register_sqlite_vec() -> Result<(), FrameworkError> {
    static RESULT: OnceLock<Result<(), i32>> = OnceLock::new();

    let result = RESULT.get_or_init(|| unsafe {
        let rc = rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut std::ffi::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> i32,
        >(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
        if rc == rusqlite::ffi::SQLITE_OK {
            Ok(())
        } else {
            Err(rc)
        }
    });

    result.map_err(|rc| {
        FrameworkError::Config(format!(
            "failed to register bundled sqlite-vec extension (sqlite3_auto_extension rc={rc})"
        ))
    })
}
