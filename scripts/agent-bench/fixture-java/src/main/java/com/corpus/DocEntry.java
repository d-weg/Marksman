package com.corpus;

public class DocEntry {
    public String name;
    public String path;
    public float score;
    public EntryKind kind;

    public DocEntry(String name, String path, float score, EntryKind kind) {
        this.name = name;
        this.path = path;
        this.score = score;
        this.kind = kind;
    }

    public String display() {
        return name + " (" + path + ")";
    }
}
