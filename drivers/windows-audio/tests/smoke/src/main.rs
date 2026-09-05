fn main() {
    // Endpoint enumeration/open verification is added with the ACX circuit.
    // This executable exists now so CI and test machines have a stable entry
    // point instead of growing ad-hoc PowerShell probes.
    eprintln!("ACX endpoint smoke test is not available until Stage 0 circuit creation lands");
    std::process::exit(2);
}
