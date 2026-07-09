package com.corpus;

import java.util.ArrayList;
import java.util.List;

public class Tokenize {
    // strip non-alphanumeric ends, lowercase
    public static String normalize(String token) {
        int start = 0;
        int end = token.length();
        while (start < end && !Character.isLetterOrDigit(token.charAt(start))) {
            start++;
        }
        while (end > start && !Character.isLetterOrDigit(token.charAt(end - 1))) {
            end--;
        }
        return token.substring(start, end).toLowerCase();
    }

    // split whitespace, normalize, drop empties
    public static List<String> tokenize(String text) {
        List<String> out = new ArrayList<>();
        for (String piece : text.split("\\s+")) {
            String norm = normalize(piece);
            if (!norm.isEmpty()) {
                out.add(norm);
            }
        }
        return out;
    }
}
