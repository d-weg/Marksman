public let RRF_K: Float = 60.0

public func blendScores(_ lexical: [(String, Float)], _ semantic: [(String, Float)]) -> [(String, Float)] {
    var fused: [String: Float] = [:]

    for (rank, entry) in lexical.enumerated() {
        fused[entry.0, default: 0.0] += 1.0 / (RRF_K + Float(rank) + 1.0)
    }
    for (rank, entry) in semantic.enumerated() {
        fused[entry.0, default: 0.0] += 1.0 / (RRF_K + Float(rank) + 1.0)
    }

    return fused
        .map { ($0.key, $0.value) }
        .sorted { $0.1 > $1.1 }
}
