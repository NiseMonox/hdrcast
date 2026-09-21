using System;
using System.Collections;
using System.Collections.Generic;
using System.Globalization;
using System.Text;

namespace HdrCast.Common
{
    /// <summary>
    /// Minimal recursive-descent JSON reader/writer (mpv JSON IPC, invites, signaling; no System.Text.Json on net48).
    /// Objects become Dictionary&lt;string, object?&gt;, arrays List&lt;object?&gt;, numbers long (integral) or double,
    /// plus string / bool / null. Tolerant of surrounding whitespace and of trailing garbage after the value.
    /// </summary>
    public static class MiniJson
    {
        public static object? Parse(string text)
        {
            if (text == null) throw new ArgumentNullException(nameof(text));
            var r = new Reader(text);
            r.SkipWs();
            object? v = r.ReadValue();
            return v;
        }

        public static bool TryParse(string text, out object? value)
        {
            try
            {
                value = Parse(text);
                return true;
            }
            catch (FormatException)
            {
                value = null;
                return false;
            }
        }

        // ---- typed accessors --------------------------------------------------------------

        public static Dictionary<string, object?>? AsObject(object? v) => v as Dictionary<string, object?>;
        public static List<object?>? AsArray(object? v) => v as List<object?>;

        public static object? Get(object? obj, string key)
        {
            var d = AsObject(obj);
            if (d == null) return null;
            return d.TryGetValue(key, out object? v) ? v : null;
        }

        public static string? GetString(object? obj, string key) => Get(obj, key) as string;

        public static double? AsDouble(object? v)
        {
            if (v is double d) return d;
            if (v is long l) return l;
            if (v is int i) return i;
            if (v is string s && double.TryParse(s, NumberStyles.Float, CultureInfo.InvariantCulture, out double p)) return p;
            return null;
        }

        public static long? AsLong(object? v)
        {
            if (v is long l) return l;
            if (v is int i) return i;
            if (v is double d && !double.IsNaN(d) && !double.IsInfinity(d)) return (long)Math.Round(d);
            if (v is string s && long.TryParse(s, NumberStyles.Integer, CultureInfo.InvariantCulture, out long p)) return p;
            return null;
        }

        public static bool? AsBool(object? v)
        {
            if (v is bool b) return b;
            if (v is string s)
            {
                if (s == "yes" || s == "true") return true;
                if (s == "no" || s == "false") return false;
            }
            return null;
        }

        // ---- writer ----------------------------------------------------------------------

        public static string Serialize(object? value)
        {
            var sb = new StringBuilder();
            WriteValue(sb, value);
            return sb.ToString();
        }

        private static void WriteValue(StringBuilder sb, object? value)
        {
            switch (value)
            {
                case null: sb.Append("null"); break;
                case string s: WriteString(sb, s); break;
                case bool b: sb.Append(b ? "true" : "false"); break;
                case int i: sb.Append(i.ToString(CultureInfo.InvariantCulture)); break;
                case long l: sb.Append(l.ToString(CultureInfo.InvariantCulture)); break;
                case float f: sb.Append(f.ToString("R", CultureInfo.InvariantCulture)); break;
                case double d: sb.Append(d.ToString("R", CultureInfo.InvariantCulture)); break;
                case decimal m: sb.Append(m.ToString(CultureInfo.InvariantCulture)); break;
                case IDictionary<string, object?> dict:
                    sb.Append('{');
                    bool firstK = true;
                    foreach (var kv in dict)
                    {
                        if (!firstK) sb.Append(',');
                        firstK = false;
                        WriteString(sb, kv.Key);
                        sb.Append(':');
                        WriteValue(sb, kv.Value);
                    }
                    sb.Append('}');
                    break;
                case IEnumerable list:
                    sb.Append('[');
                    bool first = true;
                    foreach (object? item in list)
                    {
                        if (!first) sb.Append(',');
                        first = false;
                        WriteValue(sb, item);
                    }
                    sb.Append(']');
                    break;
                default:
                    WriteString(sb, Convert.ToString(value, CultureInfo.InvariantCulture) ?? "");
                    break;
            }
        }

        public static void WriteString(StringBuilder sb, string s)
        {
            sb.Append('"');
            foreach (char c in s)
            {
                switch (c)
                {
                    case '"': sb.Append("\\\""); break;
                    case '\\': sb.Append("\\\\"); break;
                    case '\n': sb.Append("\\n"); break;
                    case '\r': sb.Append("\\r"); break;
                    case '\t': sb.Append("\\t"); break;
                    case '\b': sb.Append("\\b"); break;
                    case '\f': sb.Append("\\f"); break;
                    default:
                        if (c < 0x20)
                            sb.Append("\\u").Append(((int)c).ToString("x4", CultureInfo.InvariantCulture));
                        else
                            sb.Append(c);
                        break;
                }
            }
            sb.Append('"');
        }

        // ---- reader ----------------------------------------------------------------------

        private sealed class Reader
        {
            private readonly string _s;
            private int _i;

            public Reader(string s) { _s = s; }

            private FormatException Error(string msg) => new FormatException("JSON: " + msg + " at " + _i);

            public void SkipWs()
            {
                while (_i < _s.Length)
                {
                    char c = _s[_i];
                    if (c == ' ' || c == '\t' || c == '\n' || c == '\r') _i++;
                    else break;
                }
            }

            public object? ReadValue()
            {
                SkipWs();
                if (_i >= _s.Length) throw Error("unexpected end");
                char c = _s[_i];
                switch (c)
                {
                    case '{': return ReadObject();
                    case '[': return ReadArray();
                    case '"': return ReadString();
                    case 't': Expect("true"); return true;
                    case 'f': Expect("false"); return false;
                    case 'n': Expect("null"); return null;
                    default:
                        if (c == '-' || (c >= '0' && c <= '9')) return ReadNumber();
                        throw Error("unexpected character '" + c + "'");
                }
            }

            private void Expect(string word)
            {
                if (string.CompareOrdinal(_s, _i, word, 0, word.Length) != 0) throw Error("expected " + word);
                _i += word.Length;
            }

            private Dictionary<string, object?> ReadObject()
            {
                var d = new Dictionary<string, object?>(StringComparer.Ordinal);
                _i++; // {
                SkipWs();
                if (_i < _s.Length && _s[_i] == '}') { _i++; return d; }
                while (true)
                {
                    SkipWs();
                    if (_i >= _s.Length || _s[_i] != '"') throw Error("expected key");
                    string key = ReadString();
                    SkipWs();
                    if (_i >= _s.Length || _s[_i] != ':') throw Error("expected ':'");
                    _i++;
                    d[key] = ReadValue();
                    SkipWs();
                    if (_i >= _s.Length) throw Error("unterminated object");
                    char c = _s[_i++];
                    if (c == ',') continue;
                    if (c == '}') return d;
                    throw Error("expected ',' or '}'");
                }
            }

            private List<object?> ReadArray()
            {
                var list = new List<object?>();
                _i++; // [
                SkipWs();
                if (_i < _s.Length && _s[_i] == ']') { _i++; return list; }
                while (true)
                {
                    list.Add(ReadValue());
                    SkipWs();
                    if (_i >= _s.Length) throw Error("unterminated array");
                    char c = _s[_i++];
                    if (c == ',') continue;
                    if (c == ']') return list;
                    throw Error("expected ',' or ']'");
                }
            }

            private string ReadString()
            {
                _i++; // opening quote
                var sb = new StringBuilder();
                while (true)
                {
                    if (_i >= _s.Length) throw Error("unterminated string");
                    char c = _s[_i++];
                    if (c == '"') return sb.ToString();
                    if (c != '\\')
                    {
                        sb.Append(c);
                        continue;
                    }
                    if (_i >= _s.Length) throw Error("bad escape");
                    char e = _s[_i++];
                    switch (e)
                    {
                        case '"': sb.Append('"'); break;
                        case '\\': sb.Append('\\'); break;
                        case '/': sb.Append('/'); break;
                        case 'b': sb.Append('\b'); break;
                        case 'f': sb.Append('\f'); break;
                        case 'n': sb.Append('\n'); break;
                        case 'r': sb.Append('\r'); break;
                        case 't': sb.Append('\t'); break;
                        case 'u':
                            if (_i + 4 > _s.Length) throw Error("bad \\u escape");
                            if (!int.TryParse(_s.Substring(_i, 4), NumberStyles.HexNumber, CultureInfo.InvariantCulture, out int code))
                                throw Error("bad \\u escape");
                            _i += 4;
                            sb.Append((char)code); // surrogate pairs arrive as two consecutive \u escapes
                            break;
                        default: throw Error("unknown escape '\\" + e + "'");
                    }
                }
            }

            private object ReadNumber()
            {
                int start = _i;
                bool isFloat = false;
                if (_s[_i] == '-') _i++;
                while (_i < _s.Length)
                {
                    char c = _s[_i];
                    if (c >= '0' && c <= '9') { _i++; continue; }
                    if (c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-') { isFloat = true; _i++; continue; }
                    break;
                }
                string tok = _s.Substring(start, _i - start);
                if (!isFloat && long.TryParse(tok, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out long l)) return l;
                if (double.TryParse(tok, NumberStyles.Float, CultureInfo.InvariantCulture, out double d)) return d;
                throw Error("bad number '" + tok + "'");
            }
        }
    }
}
