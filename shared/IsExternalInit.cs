// Polyfill so `record` / `init` accessors compile for net48 (the runtime never touches this type).
#if !NET5_0_OR_GREATER
namespace System.Runtime.CompilerServices
{
    internal static class IsExternalInit { }
}
#endif
