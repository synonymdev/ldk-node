// swift-tools-version:5.9
// The swift-tools-version declares the minimum version of Swift required to build this package.

import PackageDescription

<<<<<<< HEAD
let tag = "v0.7.0-rc.66"
<<<<<<< HEAD
let checksum = "21ac13bfdc9fdd3099a688bd0053f8b14c74b5957943b2f09624886e65556a8e"
=======
let checksum = "3d4ed7123281235353cc7558317c3c15f94e0ff9adcab5fbb1fe35d572662f3d"
>>>>>>> 8fe28e1 (fix: wait for on-chain broadcast results)
=======
let tag = "v0.7.0-rc.67"
let checksum = "2bb344a67eccd18708677190104e7769b26e603695f61318e47a794010f29905"
>>>>>>> 049b116 (fix: classify broadcast backend results)
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
