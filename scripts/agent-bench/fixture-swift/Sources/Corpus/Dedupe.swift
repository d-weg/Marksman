public func collapsePaths(_ hits: [DocEntry]) -> [DocEntry] {
    var seen: Set<String> = []
    var result: [DocEntry] = []
    for hit in hits {
        if seen.insert(hit.path).inserted {
            result.append(hit)
        }
    }
    return result
}
