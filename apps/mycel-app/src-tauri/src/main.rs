//! Desktop entry; the app itself is `mycel_app_lib::run` (shared with Android).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    mycel_app_lib::run()
}
