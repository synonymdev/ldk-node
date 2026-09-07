## Generating the bindings

Run from the repository root:

```sh
./bindgen.sh
```

## Publishing a binding release

1. Update the version in `Cargo.toml`, `bindings/kotlin/ldk-node-android/gradle.properties`, `bindings/kotlin/ldk-node-jvm/gradle.properties`, `bindings/python/pyproject.toml`, and the release tag in `Package.swift`.
2. Refresh `Cargo.lock` with `cargo update -w`.
3. Update the existing Synonym fork heading and additions subsection in `CHANGELOG.md`.
4. Run `./bindgen.sh` from the repository root.
5. Commit `Cargo.lock`, the generated Swift, Kotlin Android, and Python sources, and the updated `Package.swift` checksum.
6. Push every release change before tagging the release commit.
7. Verify that `shasum -a 256 bindings/swift/LDKNodeFFI.xcframework.zip` matches the checksum in `Package.swift`.
8. Write concise, consumer-facing release notes covering only Rust, FFI, and binding changes since the previous release.
9. Publish the tag as the latest GitHub release and upload `bindings/swift/LDKNodeFFI.xcframework.zip`.
10. Add the release link to the PR description.
