@echo off
rem ASM-Speak v2 unter Windows bauen.
rem Benoetigt NASM (nasm.us) und MinGW-w64 (z.B. w64devkit oder MSYS2).
rem Die Import-Bibliotheken fuer libsodium/libopus liegen in lib\
nasm -f win64 server.asm -o server.obj || goto :fehler
nasm -f win64 client.asm -o client.obj || goto :fehler
windres client.rc -O coff -o client_res.obj || goto :fehler
gcc -nostdlib -e start -Wl,--subsystem,console server.obj -o server.exe -Llib -lkernel32 -lws2_32 -lsodium || goto :fehler
gcc -nostdlib -e start -Wl,--subsystem,windows client.obj client_res.obj -o client.exe -Llib -lkernel32 -luser32 -lgdi32 -lws2_32 -lwinmm -lsodium -lopus || goto :fehler
del *.obj
echo Fertig: server.exe und client.exe
goto :eof
:fehler
echo Build fehlgeschlagen.
