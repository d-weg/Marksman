package com.corpus;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class Store {
    public List<DocEntry> docs;
    private Map<String, List<Integer>> postings;

    public static final class Hit {
        public String id;
        public float score;

        public Hit(String id, float score) {
            this.id = id;
            this.score = score;
        }
    }

    public Store() {
        this.docs = new ArrayList<>();
        this.postings = new HashMap<>();
    }

    // push DocEntry{name,path,score:0.0,kind}; tokenize(body) into postings; return id
    public int add(String name, String path, EntryKind kind, String body) {
        int id = docs.size();
        docs.add(new DocEntry(name, path, 0.0f, kind));
        for (String token : Tokenize.tokenize(body)) {
            postings.computeIfAbsent(token, k -> new ArrayList<>()).add(id);
        }
        return id;
    }

    // ids for token, hit count as score
    public List<Hit> lookup(String token) {
        List<Hit> out = new ArrayList<>();
        List<Integer> ids = postings.get(token);
        if (ids == null) {
            return out;
        }
        // hit count as score
        Map<Integer, Float> counts = new HashMap<>();
        for (int id : ids) {
            counts.merge(id, 1.0f, Float::sum);
        }
        for (Map.Entry<Integer, Float> e : counts.entrySet()) {
            out.add(new Hit(Integer.toString(e.getKey()), e.getValue()));
        }
        return out;
    }
}
