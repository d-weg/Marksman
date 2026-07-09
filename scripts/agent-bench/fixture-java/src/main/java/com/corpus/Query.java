package com.corpus;

import java.util.ArrayList;
import java.util.List;

public class Query {
    // lexical=concat lookup per tokenize(query) token; semantic=docs enumerated (i.toString(), doc.score);
    // fused=blendScores(...); take(top*2), map id->docs[idx] into DocEntry{name,path,score,kind:SOURCE};
    // return collapsePaths(hits).take(top)
    public static List<DocEntry> search(Store store, String query, int top) {
        List<Store.Hit> lexical = new ArrayList<>();
        for (String token : Tokenize.tokenize(query)) {
            lexical.addAll(store.lookup(token));
        }

        List<Store.Hit> semantic = new ArrayList<>();
        for (int i = 0; i < store.docs.size(); i++) {
            semantic.add(new Store.Hit(Integer.toString(i), store.docs.get(i).score));
        }

        List<Store.Hit> fused = Rank.blendScores(lexical, semantic);

        List<DocEntry> hits = new ArrayList<>();
        int limit = Math.min(top * 2, fused.size());
        for (int i = 0; i < limit; i++) {
            Store.Hit hit = fused.get(i);
            int idx = Integer.parseInt(hit.id);
            DocEntry doc = store.docs.get(idx);
            hits.add(new DocEntry(doc.name, doc.path, hit.score, EntryKind.SOURCE));
        }

        List<DocEntry> collapsed = Dedupe.collapsePaths(hits);
        if (collapsed.size() > top) {
            return new ArrayList<>(collapsed.subList(0, top));
        }
        return collapsed;
    }
}
