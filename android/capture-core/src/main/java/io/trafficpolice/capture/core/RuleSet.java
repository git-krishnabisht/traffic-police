package io.trafficpolice.capture.core;

import java.io.ByteArrayOutputStream;
import java.nio.charset.Charset;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.regex.Pattern;
import java.util.regex.PatternSyntaxException;

/**
 * The host's rules, compiled (PROTOCOL.md §8). Immutable: {@code set_rules} replaces the whole
 * set, so requests in flight keep the set they started with and no lock is needed. A rule with
 * an error is left out and reported in {@code rules_ack}; the others stay active.
 */
final class RuleSet {
    static final RuleSet EMPTY = new RuleSet(null, Collections.<Rule>emptyList(), Collections.<Error>emptyList());

    static final long MAX_DELAY_MS = 10L * 60 * 1000;

    final String version;
    /** Enabled rules without errors, in list order. */
    final List<Rule> rules;
    final List<Error> errors;

    private RuleSet(String version, List<Rule> rules, List<Error> errors) {
        this.version = version;
        this.rules = rules;
        this.errors = errors;
    }

    /** A problem with one rule. */
    static final class Error {
        final String rule;
        final String field;
        final String message;

        Error(String rule, String field, String message) {
            this.rule = rule;
            this.field = field;
            this.message = message;
        }
    }

    /** One matcher: exact, glob or regex (§8.2). */
    static final class Matcher {
        private final String exact;
        private final Pattern pattern;
        private final boolean ignoreCase;

        private Matcher(String exact, Pattern pattern, boolean ignoreCase) {
            this.exact = exact;
            this.pattern = pattern;
            this.ignoreCase = ignoreCase;
        }

        boolean matches(String value) {
            if (value == null) {
                return false;
            }
            if (exact != null) {
                return ignoreCase ? exact.equalsIgnoreCase(value) : exact.equals(value);
            }
            return pattern.matcher(value).find();
        }
    }

    static final class QueryMatcher {
        final String name;
        /** Null: the parameter only has to be present. */
        final Matcher value;

        QueryMatcher(String name, Matcher value) {
            this.name = name;
            this.value = value;
        }
    }

    /** An action; {@code type} is one of delay, fail, status, header, body, replace. */
    static final class Action {
        final String type;
        long ms;
        String exception;
        String message;
        int code;
        String reason;
        String op;
        String name;
        String value;
        byte[] body;
        String contentType;
        String find;
        String with;
        Pattern regex;

        Action(String type) {
            this.type = type;
        }

        boolean editsResponse() {
            return "status".equals(type) || "header".equals(type) || editsBody();
        }

        boolean editsBody() {
            return "body".equals(type) || "replace".equals(type);
        }
    }

    static final class Rule {
        final String id;
        final String name;
        final List<String> methods = new ArrayList<>();
        String scheme;
        Matcher host;
        int port = -1;
        Matcher path;
        final List<QueryMatcher> query = new ArrayList<>();
        final List<Action> actions = new ArrayList<>();
        boolean cacheRewrites;

        Rule(String id, String name) {
            this.id = id;
            this.name = name;
        }

        boolean matches(String method, String scheme, String host, int port, String encodedPath,
                Map<String, List<String>> queryParams) {
            if (!methods.isEmpty()) {
                boolean any = false;
                for (String m : methods) {
                    any |= m.equalsIgnoreCase(method);
                }
                if (!any) {
                    return false;
                }
            }
            if (this.scheme != null && !this.scheme.equalsIgnoreCase(scheme)) {
                return false;
            }
            if (this.host != null && !this.host.matches(host)) {
                return false;
            }
            if (this.port != -1 && this.port != port) {
                return false;
            }
            if (this.path != null && !this.path.matches(encodedPath)) {
                return false;
            }
            for (QueryMatcher q : query) {
                List<String> values = queryParams == null ? null : queryParams.get(q.name);
                if (values == null || values.isEmpty()) {
                    return false;
                }
                if (q.value != null) {
                    boolean any = false;
                    for (String v : values) {
                        any |= q.value.matches(v);
                    }
                    if (!any) {
                        return false;
                    }
                }
            }
            return true;
        }
    }

    /** Enabled rules matching a request, in list order (empty when none). */
    List<Rule> matching(String method, String scheme, String host, int port, String encodedPath,
            Map<String, List<String>> query) {
        List<Rule> out = null;
        for (Rule r : rules) {
            if (r.matches(method, scheme, host, port, encodedPath, query)) {
                if (out == null) {
                    out = new ArrayList<>(2);
                }
                out.add(r);
            }
        }
        return out == null ? Collections.<Rule>emptyList() : out;
    }

    // --- compiling ---------------------------------------------------------------------------

    /** Compiles the wire form ({@code {"version":…, "rules":[…]}}); never throws. */
    static RuleSet compile(Map<String, Object> set) {
        if (set == null) {
            return EMPTY;
        }
        String version = JsonParser.str(set, "version");
        List<Object> list = JsonParser.list(set, "rules");
        List<Rule> rules = new ArrayList<>();
        List<Error> errors = new ArrayList<>();
        if (list != null) {
            for (int i = 0; i < list.size(); i++) {
                Object o = list.get(i);
                if (!(o instanceof Map)) {
                    errors.add(new Error("#" + (i + 1), null, "a rule must be an object"));
                    continue;
                }
                @SuppressWarnings("unchecked")
                Map<String, Object> r = (Map<String, Object>) o;
                String id = JsonParser.str(r, "id");
                if (id == null || id.isEmpty()) {
                    errors.add(new Error("#" + (i + 1), "id", "every rule needs an id"));
                    continue;
                }
                Boolean enabled = JsonParser.bool(r, "enabled");
                Rule rule;
                try {
                    rule = compileRule(id, r);
                } catch (Invalid e) {
                    errors.add(new Error(id, e.field, e.getMessage()));
                    continue;
                }
                if (enabled == null || enabled) {
                    rules.add(rule);
                }
            }
        }
        return new RuleSet(version, Collections.unmodifiableList(rules), Collections.unmodifiableList(errors));
    }

    /** What is wrong with a rule, and where. */
    private static final class Invalid extends Exception {
        final String field;

        Invalid(String field, String message) {
            super(message);
            this.field = field;
        }
    }

    private static Rule compileRule(String id, Map<String, Object> r) throws Invalid {
        Rule rule = new Rule(id, JsonParser.str(r, "name"));
        Boolean cache = JsonParser.bool(r, "cache_rewrites");
        rule.cacheRewrites = cache != null && cache;
        Map<String, Object> m = JsonParser.obj(r, "match");
        if (m != null) {
            List<Object> methods = JsonParser.list(m, "methods");
            if (methods != null) {
                for (Object o : methods) {
                    if (!(o instanceof String) || ((String) o).isEmpty()) {
                        throw new Invalid("match.methods", "methods must be names like GET");
                    }
                    rule.methods.add(((String) o).toUpperCase(Locale.ROOT));
                }
            }
            if (m.containsKey("scheme")) {
                String scheme = JsonParser.str(m, "scheme");
                if (!"http".equals(scheme) && !"https".equals(scheme)) {
                    throw new Invalid("match.scheme", "scheme must be http or https");
                }
                rule.scheme = scheme;
            }
            rule.host = matcher(m, "host", '.', true);
            if (m.containsKey("port")) {
                long port = JsonParser.num(m, "port", -1);
                if (port < 1 || port > 65535) {
                    throw new Invalid("match.port", "port must be 1 to 65535");
                }
                rule.port = (int) port;
            }
            rule.path = matcher(m, "path", '/', false);
            List<Object> query = JsonParser.list(m, "query");
            if (query != null) {
                for (int i = 0; i < query.size(); i++) {
                    Object o = query.get(i);
                    if (!(o instanceof Map)) {
                        throw new Invalid("match.query[" + i + "]", "a query match is {name, value}");
                    }
                    @SuppressWarnings("unchecked")
                    Map<String, Object> q = (Map<String, Object>) o;
                    String name = JsonParser.str(q, "name");
                    if (name == null || name.isEmpty()) {
                        throw new Invalid("match.query[" + i + "].name", "a query match needs a name");
                    }
                    rule.query.add(new QueryMatcher(name, matcher(q, "value", (char) 0, false)));
                }
            }
        }
        List<Object> actions = JsonParser.list(r, "actions");
        if (actions != null) {
            for (int i = 0; i < actions.size(); i++) {
                Object o = actions.get(i);
                if (!(o instanceof Map)) {
                    throw new Invalid("actions[" + i + "]", "an action must be an object");
                }
                @SuppressWarnings("unchecked")
                Map<String, Object> a = (Map<String, Object>) o;
                rule.actions.add(action(a, "actions[" + i + "]"));
            }
        }
        return rule;
    }

    /** `{exact}`, `{glob}` or `{regex}` under {@code key}, or null when absent. */
    private static Matcher matcher(Map<String, Object> parent, String key, char separator, boolean ignoreCase)
            throws Invalid {
        if (!parent.containsKey(key) || parent.get(key) == null) {
            return null;
        }
        Map<String, Object> m = JsonParser.obj(parent, key);
        String where = "match." + key;
        if (m == null || m.size() != 1) {
            throw new Invalid(where, "a matcher is exactly one of exact, glob or regex");
        }
        String exact = JsonParser.str(m, "exact");
        if (exact != null) {
            return new Matcher(exact, null, ignoreCase);
        }
        String glob = JsonParser.str(m, "glob");
        if (glob != null) {
            return new Matcher(null, globPattern(glob, separator, ignoreCase), ignoreCase);
        }
        String regex = JsonParser.str(m, "regex");
        if (regex != null) {
            try {
                return new Matcher(null, Pattern.compile(regex, ignoreCase ? Pattern.CASE_INSENSITIVE : 0), ignoreCase);
            } catch (PatternSyntaxException e) {
                throw new Invalid(where + ".regex", e.getDescription() + " near index " + e.getIndex());
            }
        }
        throw new Invalid(where, "a matcher is exactly one of exact, glob or regex");
    }

    /**
     * A glob as an anchored regex: {@code *} is any run without the separator, {@code **} any run,
     * {@code ?} one character other than the separator (with no separator: any).
     */
    static Pattern globPattern(String glob, char separator, boolean ignoreCase) {
        StringBuilder re = new StringBuilder(glob.length() * 2 + 4).append('^');
        // a backslash before a non-letter is always a literal in java.util.regex
        String notSep = separator == 0 ? "." : "[^\\" + separator + "]";
        for (int i = 0; i < glob.length(); i++) {
            char c = glob.charAt(i);
            if (c == '*') {
                if (i + 1 < glob.length() && glob.charAt(i + 1) == '*') {
                    re.append(".*");
                    i++;
                } else {
                    re.append(notSep).append('*');
                }
            } else if (c == '?') {
                re.append(notSep);
            } else if ("\\.[]{}()<>+-=!^$|:".indexOf(c) >= 0) {
                re.append('\\').append(c);
            } else {
                re.append(c);
            }
        }
        re.append('$');
        int flags = Pattern.DOTALL | (ignoreCase ? Pattern.CASE_INSENSITIVE : 0);
        return Pattern.compile(re.toString(), flags);
    }

    private static Action action(Map<String, Object> a, String where) throws Invalid {
        String type = JsonParser.str(a, "type");
        if (type == null) {
            throw new Invalid(where + ".type", "every action needs a type");
        }
        Action act = new Action(type);
        switch (type) {
            case "delay":
                act.ms = JsonParser.num(a, "ms", -1);
                if (act.ms < 0 || act.ms > MAX_DELAY_MS) {
                    throw new Invalid(where + ".ms", "delay ms must be 0 to " + MAX_DELAY_MS);
                }
                break;
            case "fail":
                act.exception = JsonParser.str(a, "exception");
                if (!Failures.KINDS.contains(act.exception)) {
                    throw new Invalid(where + ".exception", "exception must be one of " + Failures.KINDS);
                }
                act.message = JsonParser.str(a, "message");
                break;
            case "status":
                long code = JsonParser.num(a, "code", -1);
                if (code < 100 || code > 599) {
                    throw new Invalid(where + ".code", "status code must be 100 to 599");
                }
                act.code = (int) code;
                act.reason = JsonParser.str(a, "reason");
                break;
            case "header":
                act.op = JsonParser.str(a, "op");
                if (!"add".equals(act.op) && !"set".equals(act.op) && !"remove".equals(act.op)) {
                    throw new Invalid(where + ".op", "op must be add, set or remove");
                }
                act.name = JsonParser.str(a, "name");
                if (act.name == null || act.name.isEmpty() || !validHeaderName(act.name)) {
                    throw new Invalid(where + ".name", "a header name is needed (letters, digits and -_.!#$%&'*+^`|~)");
                }
                act.value = JsonParser.str(a, "value");
                if (!"remove".equals(act.op) && (act.value == null || !validHeaderValue(act.value))) {
                    throw new Invalid(where + ".value", "add and set need a value of printable ASCII (OkHttp refuses others)");
                }
                break;
            case "body":
                String text = JsonParser.str(a, "text");
                String b64 = JsonParser.str(a, "base64");
                if ((text == null) == (b64 == null)) {
                    throw new Invalid(where, "a body action has either text or base64");
                }
                if (text != null) {
                    act.body = text.getBytes(Charset.forName("UTF-8"));
                } else {
                    act.body = base64(b64);
                    if (act.body == null) {
                        throw new Invalid(where + ".base64", "not valid base64");
                    }
                }
                act.contentType = JsonParser.str(a, "content_type");
                if (act.contentType != null && !validHeaderValue(act.contentType)) {
                    throw new Invalid(where + ".content_type", "a content type is printable ASCII");
                }
                break;
            case "replace":
                act.find = JsonParser.str(a, "find");
                act.with = JsonParser.str(a, "with");
                if (act.find == null || act.find.isEmpty() || act.with == null) {
                    throw new Invalid(where, "replace needs find (not empty) and with");
                }
                Boolean regex = JsonParser.bool(a, "regex");
                if (regex != null && regex) {
                    try {
                        act.regex = Pattern.compile(act.find);
                    } catch (PatternSyntaxException e) {
                        throw new Invalid(where + ".find", e.getDescription() + " near index " + e.getIndex());
                    }
                }
                break;
            default:
                throw new Invalid(where + ".type", "unknown action type \"" + type + "\" (a newer traffic-police?)");
        }
        return act;
    }

    /** RFC 9110 token characters. */
    private static boolean validHeaderName(String name) {
        for (int i = 0; i < name.length(); i++) {
            char c = name.charAt(i);
            boolean ok = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9')
                    || "!#$%&'*+-.^_`|~".indexOf(c) >= 0;
            if (!ok) {
                return false;
            }
        }
        return true;
    }

    /** What OkHttp accepts in a header value: printable ASCII and tabs. */
    private static boolean validHeaderValue(String value) {
        for (int i = 0; i < value.length(); i++) {
            char c = value.charAt(i);
            if (c != '\t' && (c < 0x20 || c > 0x7e)) {
                return false;
            }
        }
        return true;
    }

    /** Standard base64, padded or not (java.util.Base64 needs API 26; the library runs from 21). */
    static byte[] base64(String s) {
        ByteArrayOutputStream out = new ByteArrayOutputStream(s.length() * 3 / 4 + 3);
        int buf = 0;
        int bits = 0;
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            int v;
            if (c >= 'A' && c <= 'Z') {
                v = c - 'A';
            } else if (c >= 'a' && c <= 'z') {
                v = c - 'a' + 26;
            } else if (c >= '0' && c <= '9') {
                v = c - '0' + 52;
            } else if (c == '+' || c == '-') {
                v = 62;
            } else if (c == '/' || c == '_') {
                v = 63;
            } else if (c == '=') {
                break;
            } else if (c == ' ' || c == '\n' || c == '\r' || c == '\t') {
                continue;
            } else {
                return null;
            }
            buf = (buf << 6) | v;
            bits += 6;
            if (bits >= 8) {
                bits -= 8;
                out.write((buf >> bits) & 0xff);
            }
        }
        return bits >= 6 ? null : out.toByteArray();
    }
}
