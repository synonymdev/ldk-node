## Generating the bindings

Run from the repository root:

```sh
./bindgen.sh
```

## Publishing a binding release

1. Update the version in `Cargo.toml`, `bindings/kotlin/ldk-node-android/gradle.properties`, `bindings/python/pyproject.toml`, and the release tag in `Package.swift`.
2. Update the existing Synonym fork heading and additions subsection in `CHANGELOG.md`.
3. Run `./bindgen.sh` from the repository root.
4. Commit the generated Swift, Kotlin Android, and Python sources plus the updated `Package.swift` checksum.
5. Push every release change before tagging the release commit.
6. Verify that `shasum -a 256 bindings/swift/LDKNodeFFI.xcframework.zip` matches the checksum in `Package.swift`.
7. Publish the tag as the latest GitHub release and upload `bindings/swift/LDKNodeFFI.xcframework.zip`.
8. Add the release link to the PR description.
