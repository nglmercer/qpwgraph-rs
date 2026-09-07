# Packaging and releases

Producing distributable artifacts. The release workflow builds the canonical
binary for the native tarball, Flatpak, and AppImage. Local packaging
instructions are in [`packaging/README.md`](../packaging/README.md).

## AppImage

Build the release binary and run:

```bash
bash packaging/appimage/build-appimage.sh 0.1.0 ./linuxdeploy-x86_64.AppImage
```

## Flatpak

```bash
flatpak-builder --force-clean --repo=repo builddir \
  packaging/io.github.nglmercer.qpwgraph-rs.yml
flatpak build-bundle repo qpwgraph-rs-0.1.0-x86_64.flatpak \
  io.github.nglmercer.qpwgraph-rs stable \
  --runtime-repo=https://releases.freedesktop-sdk.io/freedesktop-sdk.flatpakrepo
```

## Windows

Tagged releases also publish a portable Windows artifact named
`qpwgraph-rs-X.Y.Z-x86_64-pc-windows-msvc.zip` containing the executable,
README, and license.

The portable tier never installs or changes Windows default devices and never
requires the optional driver. A separate full bundle can be prepared with
`packaging/windows/build-full-installer.ps1`; it contains the application,
the ready driver package, and explicit install/uninstall wrappers. Production
bundles require the returned Microsoft-signed package and the Verifier, HLK,
Secure Boot, lifecycle, and client gates. Test-signed bundles are development
artifacts only and must be marked with the script's explicit
`-AllowTestSigned` switch.

The candidate/signing boundary is deliberately credential-free:

```powershell
.\drivers\windows-audio\package\build-release-driver.ps1
.\drivers\windows-audio\package\prepare-dashboard-submission.ps1 `
  -OutputDirectory .\artifacts\dashboard-submission
.\drivers\windows-audio\package\verify-returned-driver.ps1 `
  -PackageRoot .\artifacts\returned-driver `
  -SubmissionManifest .\artifacts\dashboard-submission\submission-manifest.json
```

The first two commands prepare hashes and submission material; they do not
contact Microsoft or store account credentials. The returned package must be
verified before a full bundle is built.

The evidence validator requires the retained HLK result package and hash,
test-machine build, release-candidate manifest, Secure Boot audit, Driver
Verifier collection, structured stress result, and returned-package
verification. It checks package-hash relationships and production-signature
claims but does not execute any lab test. It also requires the complete
`release-audit.ps1 -EvidencePath` acceptance record:

    .\drivers\windows-audio\package\validate-release-evidence.ps1 -EvidenceRoot .\artifacts\release-evidence -PackageRoot .\artifacts\returned-driver

The manual `.github/workflows/windows-driver-release.yml` workflow keeps the
unsigned eWDK candidate separate from the returned package. A full release
requires both an external evidence artifact containing the HLK, Secure Boot,
Verifier, stress, and returned-package verification records, and an artifact
containing the returned Microsoft-signed driver package. The retained evidence
must include the HLK result ZIP and hash, release-candidate manifest,
test-machine build, HLK preparation, Secure Boot audit, `verifier-evidence.json`,
`driver-stress.json`, and `acceptance-evidence.json`. The acceptance record must
mark the HLK, Rust runtime, Verifier, signing, Secure Boot, lifecycle,
endpoint-churn, and client gates as `pass`. Supply each artifact name together
with the workflow run ID that produced it; missing either pair fails the gate.

## Related

- [Building](building.md) — the release builds these artifacts wrap.
