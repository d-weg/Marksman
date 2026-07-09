package com.corpus;

import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Set;

public class Dedupe {
    // keep first per path
    public static List<DocEntry> collapsePaths(List<DocEntry> hits) {
        List<DocEntry> out = new ArrayList<>();
        Set<String> seen = new HashSet<>();
        for (DocEntry hit : hits) {
            if (seen.add(hit.path)) {
                out.add(hit);
            }
        }
        return out;
    }
}
