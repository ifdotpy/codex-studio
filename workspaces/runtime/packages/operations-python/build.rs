fn main() {
    // Python resolves extension symbols when it loads the module on macOS.
    pyo3_build_config::add_extension_module_link_args();
}
