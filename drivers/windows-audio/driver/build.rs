fn main() -> Result<(), wdk_build::ConfigError> {
    // This intentionally requires a real WDK/eWDK environment. The driver
    // package must never be mistaken for a normal user-mode DLL build.
    wdk_build::configure_wdk_binary_build()
}
