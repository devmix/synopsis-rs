/* Case-sensitivity shim for cross-compiling from Linux.
 *
 * usearch (include/usearch/index.hpp) does `#include <Windows.h>` with a
 * capital W. Windows filesystems are case-insensitive, so it works there;
 * on a case-sensitive Linux host, Zig's mingw headers only provide
 * lowercase `windows.h` and the include fails. This header is placed on the
 * C++ include path for the windows-gnu CI leg (see .github/workflows/
 * release.yml) and forwards to the real header.
 */
#include <windows.h>
