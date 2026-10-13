// swift-tools-version:5.9
// The swift-tools-version declares the minimum version of Swift required to build this package.

import PackageDescription

let tag = "v0.7.0-rc.66.1"
let checksum = "999ab2497ac51e07a1c6a85c694935c64f4d455b01ef1579b7bd2546d3b36dda"
let url = "https://github.com/synonymdev/ldk-node/releases/download/\(tag)/LDKNodeFFI.xcframework.zip"

let package = Package(
    name: "ldk-node",
    platforms: [
        .iOS(.v15),
        .macOS(.v12),
    ],
    products: [
        // Products define the executables and libraries a package produces, and make them visible to other packages.
        .library(
            name: "LDKNode",
            targets: ["LDKNodeFFI", "LDKNode"]),
    ],
    targets: [
        .target(
            name: "LDKNode",
            dependencies: ["LDKNodeFFI"],
            path: "./bindings/swift/Sources"
        ),
        .binaryTarget(
            name: "LDKNodeFFI",
            url: url,
            checksum: checksum
            )
    ]
)
