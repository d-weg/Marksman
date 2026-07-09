public func normalize(_ token: String) -> String {
    let trimmed = token.drop(while: { !$0.isLetter && !$0.isNumber })
    var end = trimmed.endIndex
    while end > trimmed.startIndex {
        let prev = trimmed.index(before: end)
        if trimmed[prev].isLetter || trimmed[prev].isNumber {
            break
        }
        end = prev
    }
    return String(trimmed[trimmed.startIndex..<end]).lowercased()
}

public func tokenize(_ text: String) -> [String] {
    return text
        .split(whereSeparator: { $0.isWhitespace })
        .map { normalize(String($0)) }
        .filter { !$0.isEmpty }
}
