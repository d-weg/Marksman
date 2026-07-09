public func search(_ store: Store, _ query: String, _ top: Int) -> [DocEntry] {
    var lexical: [(String, Float)] = []
    for token in tokenize(query) {
        lexical.append(contentsOf: store.lookup(token))
    }

    let semantic: [(String, Float)] = store.docs.enumerated().map { (i, doc) in
        (String(i), doc.score)
    }

    let fused = blendScores(lexical, semantic)

    var hits: [DocEntry] = []
    for (id, score) in fused.prefix(top * 2) {
        guard let idx = Int(id), idx < store.docs.count else { continue }
        let doc = store.docs[idx]
        hits.append(DocEntry(name: doc.name, path: doc.path, score: score, kind: .source))
    }

    return Array(collapsePaths(hits).prefix(top))
}
