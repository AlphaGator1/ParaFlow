// Release builds are windowed: this is a tray app, and a console flashing on
// launch would give the game away.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    flowstate_lib::run()
}
