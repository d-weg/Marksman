package com.corpus;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

public class Rank {
    public static final float RRF_K = 60.0f;

    // reciprocal-rank fusion: fused[id] += 1/(RRF_K + rank + 1) over each list; sort desc by score
    public static List<Store.Hit> blendScores(List<Store.Hit> lexical, List<Store.Hit> semantic) {
        Map<String, Float> fused = new LinkedHashMap<>();

        List<List<Store.Hit>> lists = new ArrayList<>();
        lists.add(lexical);
        lists.add(semantic);

        for (List<Store.Hit> list : lists) {
            for (int rank = 0; rank < list.size(); rank++) {
                Store.Hit hit = list.get(rank);
                float contribution = 1.0f / (RRF_K + rank + 1);
                fused.merge(hit.id, contribution, Float::sum);
            }
        }

        List<Store.Hit> out = new ArrayList<>();
        for (Map.Entry<String, Float> e : fused.entrySet()) {
            out.add(new Store.Hit(e.getKey(), e.getValue()));
        }
        out.sort((a, b) -> Float.compare(b.score, a.score));
        return out;
    }
}
