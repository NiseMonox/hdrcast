using System;
using System.IO;
using System.Runtime.Serialization.Json;
using System.Text;
using System.Xml;

namespace HdrCast.Common
{
    /// <summary>
    /// DataContractJsonSerializer helper (no NuGet dependency; keeps the net48 exe single-file).
    /// Types must carry [DataContract] / [DataMember]; use [OnDeserializing] to (re)apply defaults
    /// because the serializer never runs constructors.
    /// </summary>
    public static class Json
    {
        private static DataContractJsonSerializer Create<T>() =>
            new DataContractJsonSerializer(typeof(T), new DataContractJsonSerializerSettings
            {
                UseSimpleDictionaryFormat = true,
            });

        public static string Serialize<T>(T value, bool indent = false)
        {
            using (var ms = new MemoryStream())
            {
                Write(ms, value, indent);
                return Encoding.UTF8.GetString(ms.ToArray());
            }
        }

        public static void Write<T>(Stream stream, T value, bool indent)
        {
            var ser = Create<T>();
            if (indent)
            {
                using (XmlDictionaryWriter w = JsonReaderWriterFactory.CreateJsonWriter(stream, new UTF8Encoding(false), false, true, "  "))
                {
                    ser.WriteObject(w, value);
                    w.Flush();
                }
            }
            else
            {
                ser.WriteObject(stream, value);
            }
        }

        public static T? Deserialize<T>(string json) where T : class
        {
            if (json == null) throw new ArgumentNullException(nameof(json));
            using (var ms = new MemoryStream(Encoding.UTF8.GetBytes(json)))
            {
                return Create<T>().ReadObject(ms) as T;
            }
        }

        /// <summary>
        /// Writes atomically (unique temp file + replace) and creates the directory when needed. The temp name is
        /// unique because both apps may save the shared group.json at the same moment.
        /// </summary>
        public static void SaveFile<T>(string path, T value)
        {
            string? dir = Path.GetDirectoryName(path);
            if (!string.IsNullOrEmpty(dir)) Directory.CreateDirectory(dir);
            string tmp = path + "." + Guid.NewGuid().ToString("N").Substring(0, 8) + ".tmp";
            try
            {
                using (var fs = new FileStream(tmp, FileMode.Create, FileAccess.Write, FileShare.None))
                {
                    Write(fs, value, indent: true);
                }
                if (File.Exists(path)) File.Replace(tmp, path, null);
                else File.Move(tmp, path);
            }
            finally
            {
                try { if (File.Exists(tmp)) File.Delete(tmp); } catch (IOException) { } catch (UnauthorizedAccessException) { }
            }
        }

        /// <summary>Returns null when the file is missing or unreadable (callers fall back to defaults).</summary>
        public static T? LoadFile<T>(string path) where T : class
        {
            try
            {
                if (!File.Exists(path)) return null;
                return Deserialize<T>(File.ReadAllText(path, Encoding.UTF8));
            }
            catch (Exception)
            {
                return null;
            }
        }
    }
}
