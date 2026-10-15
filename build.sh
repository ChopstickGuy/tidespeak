#!/bin/sh
# Bauen unter Linux (Cross-Compile fuer Windows): nasm + mingw-w64 binutils
set -e
L="${MINGW_LIB:+-L$MINGW_LIB} -Llib"
nasm -f win64 server.asm -o server.obj
nasm -f win64 client.asm -o client.obj
x86_64-w64-mingw32-windres --preprocessor=cat client.rc -O coff -o client_res.obj
x86_64-w64-mingw32-ld server.obj -o server.exe -e start --subsystem console $L -lkernel32 -lws2_32 -lsodium
x86_64-w64-mingw32-ld client.obj client_res.obj -o client.exe -e start --subsystem windows $L \
    -lkernel32 -luser32 -lgdi32 -lws2_32 -lwinmm -lsodium -lopus
rm -f *.obj
echo "OK: server.exe client.exe"
