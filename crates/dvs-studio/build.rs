fn main() {
    // Generates the context the Tauri runtime needs: the parsed config, the capability
    // set, and the embedded frontend from `ui/`. Without it `tauri::generate_context!`
    // has nothing to read.
    tauri_build::build();
}
