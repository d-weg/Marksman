// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "Corpus",
    products: [
        .library(name: "Corpus", targets: ["Corpus"]),
    ],
    targets: [
        .target(name: "Corpus", path: "Sources/Corpus"),
    ]
)
