public enum EntryKind {
    case source
    case doc
    case config
}

public struct DocEntry {
    public let name: String
    public let path: String
    public var score: Float
    public let kind: EntryKind

    public init(name: String, path: String, score: Float, kind: EntryKind) {
        self.name = name
        self.path = path
        self.score = score
        self.kind = kind
    }

    public func display() -> String {
        return "\(name) (\(path))"
    }
}
