public class Store {
    public var docs: [DocEntry]
    private var postings: [String: [Int]]

    public init() {
        self.docs = []
        self.postings = [:]
    }

    public func add(name: String, path: String, kind: EntryKind, body: String) -> Int {
        let id = docs.count
        docs.append(DocEntry(name: name, path: path, score: 0.0, kind: kind))
        for token in tokenize(body) {
            postings[token, default: []].append(id)
        }
        return id
    }

    public func lookup(_ token: String) -> [(String, Float)] {
        guard let ids = postings[token] else {
            return []
        }
        let count = Float(ids.count)
        return ids.map { (String($0), count) }
    }
}
