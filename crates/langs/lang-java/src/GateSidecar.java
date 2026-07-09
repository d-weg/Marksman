// GateSidecar — the RESIDENT javax.tools gate behind lang-java (embedded via include_str!,
// materialized to a temp dir, launched as `java GateSidecar.java` — JEP 330 single-file mode,
// so any JDK 17+ runs it with no build step and no dependencies).
//
// Protocol: JSON lines over stdio, one request line -> one response line.
//   request  = {"files":[{"path":"<repo-relative>","content":"<buffer>"}],
//               "classpath":"<path list>","sourcepath":"<path list>"}
//   response = {"diagnostics":[{"kind":"ERROR","source":"<path>","line":N,"col":N,
//               "code":"compiler.err.…","message":"…"}]}   or   {"error":"…"}
//
// The buffers are in-memory overlays (the VFS staging the shared spine gates on) — disk is
// consulted only through -sourcepath for types the overlay set references but doesn't carry.
// Diagnostics come STRUCTURED from a DiagnosticListener (kind/source/position/code), never
// parsed out of compiler text: javac has no structured CLI output, but javax.tools IS javac
// in-process, so verdicts can't diverge from the real compiler. An empty file set answers
// immediately (the prewarm ping that pays the JVM start off-thread).
import java.io.BufferedReader;
import java.io.FileDescriptor;
import java.io.FileOutputStream;
import java.io.InputStreamReader;
import java.io.PrintStream;
import java.io.PrintWriter;
import java.io.StringWriter;
import java.net.URI;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import javax.tools.Diagnostic;
import javax.tools.DiagnosticCollector;
import javax.tools.JavaCompiler;
import javax.tools.JavaFileObject;
import javax.tools.SimpleJavaFileObject;
import javax.tools.StandardJavaFileManager;
import javax.tools.ToolProvider;

public class GateSidecar {
    public static void main(String[] args) throws Exception {
        JavaCompiler compiler = ToolProvider.getSystemJavaCompiler();
        PrintStream out = new PrintStream(new FileOutputStream(FileDescriptor.out), true, "UTF-8");
        if (compiler == null) {
            // A JRE-only runtime has no compiler: report it per request instead of dying silently.
            out.println("{\"error\":\"no system Java compiler available (JRE instead of JDK?)\"}");
            return;
        }
        // Class output goes to a throwaway dir (never on any lookup path): the gate wants
        // diagnostics, not artifacts, but javac insists on somewhere to write.
        Path classOut = Files.createTempDirectory("marksman-java-gate");
        classOut.toFile().deleteOnExit();
        BufferedReader in = new BufferedReader(new InputStreamReader(System.in, StandardCharsets.UTF_8));
        String line;
        while ((line = in.readLine()) != null) {
            if (line.isBlank()) {
                continue;
            }
            try {
                out.println(handle(compiler, classOut, line));
            } catch (Exception e) {
                out.println("{\"error\":" + quote(String.valueOf(e)) + "}");
            }
        }
    }

    @SuppressWarnings("unchecked")
    static String handle(JavaCompiler compiler, Path classOut, String request) throws Exception {
        Map<String, Object> req = (Map<String, Object>) Json.parse(request);
        List<Object> files = (List<Object>) req.getOrDefault("files", List.of());
        List<JavaFileObject> units = new ArrayList<>();
        for (Object f : files) {
            Map<String, Object> file = (Map<String, Object>) f;
            String content = file.get("content") == null ? "" : (String) file.get("content");
            // An EMPTY buffer is the spine's deletion stand-in: keeping it as an (empty, valid)
            // unit means consumers of the deleted class fail deterministically, exactly as they
            // would once the deletion commits.
            units.add(new StringSource((String) file.get("path"), content));
        }
        if (units.isEmpty()) {
            return "{\"diagnostics\":[]}"; // the prewarm ping
        }
        List<String> options = new ArrayList<>(List.of("-proc:none", "-d", classOut.toString()));
        String classpath = str(req.get("classpath"));
        if (!classpath.isEmpty()) {
            options.add("-classpath");
            options.add(classpath);
        }
        String sourcepath = str(req.get("sourcepath"));
        if (!sourcepath.isEmpty()) {
            options.add("-sourcepath");
            options.add(sourcepath);
        }
        DiagnosticCollector<JavaFileObject> diags = new DiagnosticCollector<>();
        StandardJavaFileManager fm = compiler.getStandardFileManager(diags, Locale.ROOT, StandardCharsets.UTF_8);
        try {
            // Ignore the boolean verdict: the caller diffs the DIAGNOSTICS against a baseline
            // (pre-existing breakage must not block an unrelated edit — contract clause 5).
            StringWriter extra = new StringWriter();
            compiler.getTask(new PrintWriter(extra), fm, diags, options, null, units).call();
        } finally {
            fm.close();
        }
        StringBuilder sb = new StringBuilder("{\"diagnostics\":[");
        boolean first = true;
        for (Diagnostic<? extends JavaFileObject> d : diags.getDiagnostics()) {
            if (!first) {
                sb.append(',');
            }
            first = false;
            String source = d.getSource() == null ? "" : d.getSource().getName();
            sb.append("{\"kind\":").append(quote(d.getKind().name()))
                .append(",\"source\":").append(quote(source))
                .append(",\"line\":").append(Math.max(d.getLineNumber(), 0))
                .append(",\"col\":").append(Math.max(d.getColumnNumber(), 0))
                .append(",\"code\":").append(quote(d.getCode() == null ? "" : d.getCode()))
                .append(",\"message\":").append(quote(d.getMessage(Locale.ROOT)))
                .append('}');
        }
        return sb.append("]}").toString();
    }

    static String str(Object v) {
        return v == null ? "" : (String) v;
    }

    static String quote(String s) {
        StringBuilder sb = new StringBuilder("\"");
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"' -> sb.append("\\\"");
                case '\\' -> sb.append("\\\\");
                case '\n' -> sb.append("\\n");
                case '\r' -> sb.append("\\r");
                case '\t' -> sb.append("\\t");
                default -> {
                    if (c < 0x20) {
                        sb.append(String.format("\\u%04x", (int) c));
                    } else {
                        sb.append(c);
                    }
                }
            }
        }
        return sb.append('"').toString();
    }
}

/** One overlay buffer as a compilation unit; getName() keeps the caller's repo-relative path
 *  so diagnostics land back on the path the edit spine knows. */
class StringSource extends SimpleJavaFileObject {
    private final String path;
    private final String content;

    StringSource(String path, String content) {
        super(URI.create("string:///" + path.replace(" ", "%20")), Kind.SOURCE);
        this.path = path;
        this.content = content;
    }

    @Override
    public String getName() {
        return path;
    }

    @Override
    public CharSequence getCharContent(boolean ignoreEncodingErrors) {
        return content;
    }
}

/** Minimal JSON reader (objects/arrays/strings/numbers/literals) — the input is produced by
 *  serde_json on the Rust side, so standard JSON is the whole grammar. No dependencies, per
 *  the single-file constraint. */
class Json {
    private final String s;
    private int i;

    private Json(String s) {
        this.s = s;
    }

    static Object parse(String text) {
        Json j = new Json(text);
        j.ws();
        Object v = j.value();
        j.ws();
        if (j.i != text.length()) {
            throw new IllegalArgumentException("trailing JSON at " + j.i);
        }
        return v;
    }

    private Object value() {
        char c = peek();
        return switch (c) {
            case '{' -> object();
            case '[' -> array();
            case '"' -> string();
            case 't' -> literal("true", Boolean.TRUE);
            case 'f' -> literal("false", Boolean.FALSE);
            case 'n' -> literal("null", null);
            default -> number();
        };
    }

    private Map<String, Object> object() {
        expect('{');
        Map<String, Object> m = new java.util.LinkedHashMap<>();
        ws();
        if (peek() == '}') {
            i++;
            return m;
        }
        while (true) {
            ws();
            String k = string();
            ws();
            expect(':');
            ws();
            m.put(k, value());
            ws();
            char c = next();
            if (c == '}') {
                return m;
            }
            if (c != ',') {
                throw new IllegalArgumentException("expected , or } at " + (i - 1));
            }
        }
    }

    private List<Object> array() {
        expect('[');
        List<Object> l = new ArrayList<>();
        ws();
        if (peek() == ']') {
            i++;
            return l;
        }
        while (true) {
            ws();
            l.add(value());
            ws();
            char c = next();
            if (c == ']') {
                return l;
            }
            if (c != ',') {
                throw new IllegalArgumentException("expected , or ] at " + (i - 1));
            }
        }
    }

    private String string() {
        expect('"');
        StringBuilder sb = new StringBuilder();
        while (true) {
            char c = next();
            if (c == '"') {
                return sb.toString();
            }
            if (c != '\\') {
                sb.append(c);
                continue;
            }
            char e = next();
            switch (e) {
                case '"' -> sb.append('"');
                case '\\' -> sb.append('\\');
                case '/' -> sb.append('/');
                case 'b' -> sb.append('\b');
                case 'f' -> sb.append('\f');
                case 'n' -> sb.append('\n');
                case 'r' -> sb.append('\r');
                case 't' -> sb.append('\t');
                case 'u' -> {
                    sb.append((char) Integer.parseInt(s.substring(i, i + 4), 16));
                    i += 4;
                }
                default -> throw new IllegalArgumentException("bad escape \\" + e);
            }
        }
    }

    private Object number() {
        int start = i;
        while (i < s.length() && "+-.eE0123456789".indexOf(s.charAt(i)) >= 0) {
            i++;
        }
        return Double.parseDouble(s.substring(start, i));
    }

    private Object literal(String lit, Object v) {
        if (!s.startsWith(lit, i)) {
            throw new IllegalArgumentException("bad literal at " + i);
        }
        i += lit.length();
        return v;
    }

    private void ws() {
        while (i < s.length() && Character.isWhitespace(s.charAt(i))) {
            i++;
        }
    }

    private char peek() {
        if (i >= s.length()) {
            throw new IllegalArgumentException("unexpected end of JSON");
        }
        return s.charAt(i);
    }

    private char next() {
        char c = peek();
        i++;
        return c;
    }

    private void expect(char c) {
        if (next() != c) {
            throw new IllegalArgumentException("expected " + c + " at " + (i - 1));
        }
    }
}
