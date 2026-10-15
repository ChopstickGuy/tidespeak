; =============================================================================
; ASM-Speak Server v2 - verschluesselter Text- und Sprachchat
; Windows x64, NASM, libsodium
;
;   TCP 9987: Verbindungsaufbau + Text (secretstream XChaCha20-Poly1305)
;   UDP 9987: Opus-Sprachpakete (ChaCha20-Poly1305), werden pro Empfaenger
;             neu verschluesselt weitergeleitet
;
; Aufbau: Hauptthread nimmt Verbindungen an, je Client ein Thread,
;         ein UDP-Thread fuer Sprache. Gemeinsame Daten schuetzt g_lock.
; Der Server-Schluessel liegt in server_schluessel.bin (beim 1. Start erzeugt).
; =============================================================================

%include "common.inc"
%include "proto.inc"

%define MAX_CLIENTS     MAX_SLOTS
%define NAME_MAX        32
%define MAX_TEXT        1100

; --- Aufbau eines Client-Eintrags ---------------------------------------------
%define CL_SOCK     0       ; SOCKET
%define CL_ACTIVE   8       ; Platz belegt
%define CL_NAMED    9       ; hat Namen -> bekommt Nachrichten
%define CL_UDPOK    10      ; UDP-Adresse bestaetigt
%define CL_SLOT     11      ; eigene Nummer
%define CL_ADDR     16      ; sockaddr_in fuer UDP
%define CL_URX      32      ; UDP-Schluessel Client->Server
%define CL_UTX      64      ; UDP-Schluessel Server->Client
%define CL_URXCTR   96      ; hoechster empfangener Zaehler (Replay-Schutz)
%define CL_UTXCTR   104     ; Sendezaehler
%define CL_NAME     112     ; UTF-8, max. 32 Byte + 0
%define CL_PUSH     160
%define CL_PULL     224
%define CL_RX       288
%define CL_TX       320
%define CL_CPK      352
%define CL_EPK      384
%define CL_ESK      416
%define CL_HS       448     ; Antwort: SignPK | EphPK | Signatur
%define CL_HDR      576
%define CL_RBUF     608     ; MAX_PLAIN+1
%define CL_SBUF     1824
%define CLIENT_SIZE 3200

%define SOL_SOCKET      0xFFFF
%define SO_SNDTIMEO     0x1005
%define SO_RCVTIMEO     0x1006
%define TCP_NODELAY     1
%define GENERIC_READ    0x80000000
%define GENERIC_WRITE   0x40000000
%define CREATE_NEW      1
%define OPEN_EXISTING   3

extern CreateThread, CloseHandle, CreateFileA
extern InitializeCriticalSection, EnterCriticalSection, LeaveCriticalSection
extern WSAStartup, socket, bind, listen, accept, closesocket, setsockopt
extern recvfrom, sendto

section .data
txt_banner      db "=== ASM-Speak Server v2 (verschluesselt) ===",13,10
                db "TCP + UDP Port 9987  |  libsodium + Opus  |  Beenden: Strg+C",13,10,13,10,0
txt_err_sodium  db "Fehler: libsodium konnte nicht gestartet werden (libsodium-26.dll?)",13,10,0
txt_err_key     db "Fehler: server_schluessel.bin ist beschaedigt",13,10,0
txt_err_wsa     db "Fehler: WSAStartup fehlgeschlagen",13,10,0
txt_err_sock    db "Fehler: Socket konnte nicht erstellt werden",13,10,0
txt_err_bind    db "Fehler: Port 9987 ist belegt (laeuft der Server schon?)",13,10,0
txt_newkey      db "Neuer Server-Schluessel erzeugt: server_schluessel.bin",13,10,0
txt_fp          db "Server-Fingerabdruck: ",0
txt_fp2         db 13,10,"(Clients zeigen denselben Wert an - so erkennen sie diesen Server)",13,10,13,10,0
txt_conn        db "[Server] Verbindung angenommen",13,10,0
txt_full        db "[Server] Server voll, Verbindung abgelehnt",13,10,0
txt_hsfail      db "[Server] Verbindungsaufbau fehlgeschlagen (kein ASM-Speak-Client?)",13,10,0
txt_disc        db "[Server] Client getrennt",13,10,0
txt_crlf        db 13,10,0
txt_stars       db "*** ",0
txt_joined      db " ist beigetreten",0
txt_left        db " hat den Server verlassen",0
txt_lbr         db "[",0
txt_rbr         db "] ",0
txt_guest       db "Gast",0
key_file        db "server_schluessel.bin",0

bind_addr       dw AF_INET
                dw PORT_BE
                dd 0
                dq 0

section .bss
alignb 16
wsadata     resb 512
g_lock      resb 64
tcp_sock    resq 1
udp_sock    resq 1
sign_pk     resb 32
sign_sk     resb 64
fp_text     resb 32
key_io      resd 2
u_fromlen   resd 2
alignb 16
u_from      resb 16
u_in        resb 2048
u_plain     resb 2048
u_out       resb 2048
list_buf    resb 2048
alignb 16
clients     resb CLIENT_SIZE * MAX_CLIENTS

section .text
global start

start:
    sub rsp, 104
    call init_console
    lea rcx, [txt_banner]
    call print

    call sodium_init
    test eax, eax
    js .err_sodium
    call load_or_create_key

    lea rcx, [txt_fp]
    call print
    lea rcx, [sign_pk]
    lea rdx, [fp_text]
    call fingerprint
    lea rcx, [fp_text]
    call print
    lea rcx, [txt_fp2]
    call print

    lea rcx, [g_lock]
    call InitializeCriticalSection

    ; Slot-Nummern eintragen
    lea rax, [clients]
    xor ecx, ecx
.slots:
    mov [rax+CL_SLOT], cl
    add rax, CLIENT_SIZE
    inc ecx
    cmp ecx, MAX_CLIENTS
    jb .slots

    mov ecx, 0x0202
    lea rdx, [wsadata]
    call WSAStartup
    test eax, eax
    jnz .err_wsa

    mov ecx, AF_INET
    mov edx, SOCK_STREAM
    mov r8d, IPPROTO_TCP
    call socket
    cmp rax, -1
    je .err_sock
    mov [tcp_sock], rax
    mov rcx, rax
    lea rdx, [bind_addr]
    mov r8d, 16
    call bind
    test eax, eax
    jnz .err_bind
    mov rcx, [tcp_sock]
    mov edx, 16
    call listen

    mov ecx, AF_INET
    mov edx, SOCK_DGRAM
    mov r8d, IPPROTO_UDP
    call socket
    cmp rax, -1
    je .err_sock
    mov [udp_sock], rax
    mov rcx, rax
    lea rdx, [bind_addr]
    mov r8d, 16
    call bind
    test eax, eax
    jnz .err_bind
    mov rcx, [udp_sock]
    call udp_no_connreset

    lea rcx, [udp_thread]
    xor edx, edx
    call start_thread

; --- Verbindungen annehmen -----------------------------------------------------
.accept:
    mov rcx, [tcp_sock]
    xor edx, edx
    xor r8d, r8d
    call accept
    cmp rax, -1
    je .accept
    mov r12, rax

    lea rcx, [g_lock]
    call EnterCriticalSection
    lea rbx, [clients]
    mov r13d, MAX_CLIENTS
.find:
    cmp byte [rbx+CL_ACTIVE], 0
    je .found
    add rbx, CLIENT_SIZE
    dec r13d
    jnz .find
    lea rcx, [g_lock]
    call LeaveCriticalSection
    mov rcx, r12
    call closesocket
    lea rcx, [txt_full]
    call print
    jmp .accept
.found:
    mov [rbx+CL_SOCK], r12
    mov byte [rbx+CL_ACTIVE], 1
    mov byte [rbx+CL_NAMED], 0
    mov byte [rbx+CL_UDPOK], 0
    mov qword [rbx+CL_URXCTR], 0
    mov qword [rbx+CL_UTXCTR], 0
    lea rcx, [g_lock]
    call LeaveCriticalSection

    lea rcx, [txt_conn]
    call print
    lea rcx, [client_thread]
    mov rdx, rbx
    call start_thread
    jmp .accept

.err_sodium:
    lea rcx, [txt_err_sodium]
    call die
.err_wsa:
    lea rcx, [txt_err_wsa]
    call die
.err_sock:
    lea rcx, [txt_err_sock]
    call die
.err_bind:
    lea rcx, [txt_err_bind]
    call die

; -----------------------------------------------------------------------------
; start_thread - rcx = Funktion, rdx = Parameter
; -----------------------------------------------------------------------------
start_thread:
    sub rsp, 56
    mov r8, rcx
    mov r9, rdx
    xor ecx, ecx
    xor edx, edx
    mov qword [rsp+32], 0
    mov qword [rsp+40], 0
    call CreateThread
    mov rcx, rax
    call CloseHandle
    add rsp, 56
    ret

; -----------------------------------------------------------------------------
; set_timeout - rcx = Socket, edx = Option, r8d = Millisekunden
; -----------------------------------------------------------------------------
set_timeout:
    sub rsp, 56
    mov [rsp+48], r8d
    mov r8d, edx
    mov edx, SOL_SOCKET
    lea r9, [rsp+48]
    mov qword [rsp+32], 4
    call setsockopt
    add rsp, 56
    ret

; -----------------------------------------------------------------------------
; load_or_create_key - laedt den Ed25519-Schluessel oder erzeugt einen neuen
; -----------------------------------------------------------------------------
load_or_create_key:
    push rbx
    sub rsp, 64
    lea rcx, [key_file]
    mov edx, GENERIC_READ
    xor r8d, r8d
    xor r9d, r9d
    mov qword [rsp+32], OPEN_EXISTING
    mov qword [rsp+40], 0
    mov qword [rsp+48], 0
    call CreateFileA
    cmp rax, -1
    je .create
    mov rbx, rax
    mov rcx, rax
    lea rdx, [sign_sk]
    mov r8d, SIGN_SK
    lea r9, [key_io]
    mov qword [rsp+32], 0
    call ReadFile
    mov rcx, rbx
    call CloseHandle
    cmp dword [key_io], SIGN_SK
    jne .bad
    lea rcx, [sign_pk]
    lea rdx, [sign_sk]
    call crypto_sign_ed25519_sk_to_pk
    jmp .ret
.create:
    lea rcx, [sign_pk]
    lea rdx, [sign_sk]
    call crypto_sign_keypair
    lea rcx, [key_file]
    mov edx, GENERIC_WRITE
    xor r8d, r8d
    xor r9d, r9d
    mov qword [rsp+32], CREATE_NEW
    mov qword [rsp+40], 0
    mov qword [rsp+48], 0
    call CreateFileA
    cmp rax, -1
    je .ret
    mov rbx, rax
    mov rcx, rax
    lea rdx, [sign_sk]
    mov r8d, SIGN_SK
    lea r9, [key_io]
    mov qword [rsp+32], 0
    call WriteFile
    mov rcx, rbx
    call CloseHandle
    lea rcx, [txt_newkey]
    call print
.ret:
    add rsp, 64
    pop rbx
    ret
.bad:
    lea rcx, [txt_err_key]
    call die

; =============================================================================
; client_thread - rcx = Client-Eintrag. Verbindungsaufbau, dann Nachrichten.
; =============================================================================
client_thread:
    push rbx
    push rsi
    push rdi
    push r12
    push r13
    sub rsp, 64
    mov rbx, rcx

    mov rcx, [rbx+CL_SOCK]          ; Verbindungsaufbau max. 10 s
    mov edx, SO_RCVTIMEO
    mov r8d, 10000
    call set_timeout
    mov rcx, [rbx+CL_SOCK]          ; haengende Empfaenger bremsen nicht ewig
    mov edx, SO_SNDTIMEO
    mov r8d, 3000
    call set_timeout
    mov dword [rsp+48], 1
    mov rcx, [rbx+CL_SOCK]
    mov edx, IPPROTO_TCP
    mov r8d, TCP_NODELAY
    lea r9, [rsp+48]
    mov qword [rsp+32], 4
    call setsockopt

    mov rcx, rbx
    call server_handshake
    test eax, eax
    jz .hs_fail

    mov rcx, [rbx+CL_SOCK]          ; danach: kein Empfangs-Timeout
    mov edx, SO_RCVTIMEO
    xor r8d, r8d
    call set_timeout

    ; Willkommen: eigene Slot-Nummer (fuer UDP)
    mov byte [rsp+48], MSG_WELCOME
    mov al, [rbx+CL_SLOT]
    mov [rsp+49], al
    lea rcx, [g_lock]
    call EnterCriticalSection
    mov rcx, [rbx+CL_SOCK]
    lea rdx, [rbx+CL_PUSH]
    lea r8, [rsp+48]
    mov r9d, 2
    call ss_send
    lea rcx, [g_lock]
    call LeaveCriticalSection

.loop:
    mov rcx, [rbx+CL_SOCK]
    lea rdx, [rbx+CL_PULL]
    lea r8, [rbx+CL_RBUF]
    call ss_recv
    test rax, rax
    js .gone
    jz .loop
    mov r12, rax
    lea rsi, [rbx+CL_RBUF+1]        ; Daten hinter dem Typ-Byte
    lea r13, [r12-1]                ; Datenlaenge
    mov al, [rbx+CL_RBUF]
    cmp al, MSG_NAME
    je .name
    cmp al, MSG_TEXT
    je .text
    jmp .loop

.name:
    cmp byte [rbx+CL_NAMED], 0
    jne .loop
    mov rcx, rsi
    mov rdx, r13
    call sanitize
    lea rcx, [rbx+CL_NAME]
    mov rdx, rsi
    mov r8, r13
    call copy_name
    test eax, eax
    jnz .name_ok
    lea rcx, [rbx+CL_NAME]
    lea rdx, [txt_guest]
    call strcat_to
.name_ok:
    lea rcx, [g_lock]
    call EnterCriticalSection
    mov byte [rbx+CL_NAMED], 1
    lea rcx, [g_lock]
    call LeaveCriticalSection
    lea rdx, [txt_joined]
    call announce
    call send_userlist
    jmp .loop

.text:
    cmp byte [rbx+CL_NAMED], 0
    je .loop
    test r13, r13
    jz .loop
    cmp r13, MAX_TEXT               ; ueberlange Nachrichten kuerzen
    jbe .len_ok
    mov r13d, MAX_TEXT
    mov byte [rsi+r13], 0
.len_ok:
    mov rcx, rsi
    mov rdx, r13
    call sanitize
    lea rdi, [rbx+CL_SBUF]          ; "M[Name] Text"
    mov byte [rdi], MSG_TEXT
    lea rcx, [rdi+1]
    lea rdx, [txt_lbr]
    call strcat_to
    mov rcx, rax
    lea rdx, [rbx+CL_NAME]
    call strcat_to
    mov rcx, rax
    lea rdx, [txt_rbr]
    call strcat_to
    mov rcx, rax
    mov rdx, rsi
    call strcat_to
    mov rcx, rdi
    call log_and_broadcast
    jmp .loop

.gone:
    cmp byte [rbx+CL_NAMED], 0
    je .close
    lea rcx, [g_lock]
    call EnterCriticalSection
    mov byte [rbx+CL_NAMED], 0
    mov byte [rbx+CL_UDPOK], 0
    lea rcx, [g_lock]
    call LeaveCriticalSection
    lea rdx, [txt_left]
    call announce
    call send_userlist
    jmp .close
.hs_fail:
    lea rcx, [txt_hsfail]
    call print
.close:
    mov rcx, [rbx+CL_SOCK]
    call closesocket
    lea rcx, [rbx+CL_URX]           ; Schluesselmaterial loeschen
    mov edx, CL_RBUF - CL_URX
    call sodium_memzero
    lea rcx, [g_lock]
    call EnterCriticalSection
    mov byte [rbx+CL_NAMED], 0
    mov byte [rbx+CL_UDPOK], 0
    mov byte [rbx+CL_ACTIVE], 0     ; Platz erst ganz am Ende freigeben
    lea rcx, [g_lock]
    call LeaveCriticalSection
    lea rcx, [txt_disc]
    call print
    xor eax, eax
    add rsp, 64
    pop r13
    pop r12
    pop rdi
    pop rsi
    pop rbx
    ret

; announce - rbx = Client, rdx = Text dahinter -> "*** Name <Text>" an alle
announce:
    push rdi
    push rsi
    sub rsp, 40
    mov rsi, rdx
    lea rdi, [rbx+CL_SBUF]
    mov byte [rdi], MSG_TEXT
    lea rcx, [rdi+1]
    lea rdx, [txt_stars]
    call strcat_to
    mov rcx, rax
    lea rdx, [rbx+CL_NAME]
    call strcat_to
    mov rcx, rax
    mov rdx, rsi
    call strcat_to
    mov rcx, rdi
    call log_and_broadcast
    add rsp, 40
    pop rsi
    pop rdi
    ret

; log_and_broadcast - rcx = "M..." nullterminiert: auf Konsole + an alle
log_and_broadcast:
    push rbx
    push rsi
    push rdi
    push r12
    sub rsp, 40
    mov rsi, rcx
    lea rcx, [rsi+1]
    call print
    lea rcx, [txt_crlf]
    call print
    mov rcx, rsi
    call strlen
    mov edi, eax
    lea rcx, [g_lock]
    call EnterCriticalSection
    lea rbx, [clients]
    mov r12d, MAX_CLIENTS
.l: cmp byte [rbx+CL_NAMED], 0
    je .n
    mov rcx, [rbx+CL_SOCK]
    lea rdx, [rbx+CL_PUSH]
    mov r8, rsi
    mov r9d, edi
    call ss_send
.n: add rbx, CLIENT_SIZE
    dec r12d
    jnz .l
    lea rcx, [g_lock]
    call LeaveCriticalSection
    add rsp, 40
    pop r12
    pop rdi
    pop rsi
    pop rbx
    ret

; send_userlist - "U" + {Slot, Laenge, Name} fuer alle -> an alle
send_userlist:
    push rbx
    push rsi
    push rdi
    push r12
    push r13
    sub rsp, 32
    lea rcx, [g_lock]
    call EnterCriticalSection
    lea rdi, [list_buf]
    mov byte [rdi], MSG_USERS
    inc rdi
    lea rbx, [clients]
    mov r12d, MAX_CLIENTS
.b: cmp byte [rbx+CL_NAMED], 0
    je .bn
    mov al, [rbx+CL_SLOT]
    mov [rdi], al
    lea rcx, [rbx+CL_NAME]
    call strlen
    mov [rdi+1], al
    lea rsi, [rbx+CL_NAME]
    mov ecx, eax
    add rdi, 2
    rep movsb
.bn:
    add rbx, CLIENT_SIZE
    dec r12d
    jnz .b
    lea rax, [list_buf]
    sub rdi, rax
    mov r13d, edi
    lea rbx, [clients]
    mov r12d, MAX_CLIENTS
.s: cmp byte [rbx+CL_NAMED], 0
    je .sn
    mov rcx, [rbx+CL_SOCK]
    lea rdx, [rbx+CL_PUSH]
    lea r8, [list_buf]
    mov r9d, r13d
    call ss_send
.sn:
    add rbx, CLIENT_SIZE
    dec r12d
    jnz .s
    lea rcx, [g_lock]
    call LeaveCriticalSection
    add rsp, 32
    pop r13
    pop r12
    pop rdi
    pop rsi
    pop rbx
    ret

; sanitize - rcx = Text, rdx = Laenge: Steuerzeichen -> Leerzeichen
sanitize:
    test rdx, rdx
    jz .d
.l: cmp byte [rcx], 32
    jae .n
    mov byte [rcx], ' '
.n: inc rcx
    dec rdx
    jnz .l
.d: ret

; copy_name - rcx = Ziel, rdx = Quelle, r8 = Laenge -> eax = kopierte Laenge
; Schneidet bei NAME_MAX ab, ohne ein UTF-8-Zeichen zu zerteilen.
copy_name:
    cmp r8, NAME_MAX
    jbe .ok
    mov r8d, NAME_MAX
.cut:
    test r8d, r8d
    jz .ok
    mov al, [rdx+r8]
    and al, 0xC0
    cmp al, 0x80                    ; Folgebyte -> weiter zurueck
    jne .ok
    dec r8d
    jmp .cut
.ok:
    xor eax, eax
.l: cmp eax, r8d
    jae .d
    mov r9b, [rdx+rax]
    mov [rcx+rax], r9b
    inc eax
    jmp .l
.d: mov byte [rcx+rax], 0
    ret

; =============================================================================
; server_handshake - rcx = Client-Eintrag -> eax 1 = sicher verbunden
; =============================================================================
server_handshake:
    push rbx
    push rsi
    push rdi
    sub rsp, 48
    mov rbx, rcx
    mov rsi, [rbx+CL_SOCK]

    mov rcx, rsi                    ; 1. Client-Ephemeral empfangen
    lea rdx, [rbx+CL_CPK]
    mov r8d, KEY_BYTES
    call recv_all
    test eax, eax
    jz .fail

    lea rcx, [rbx+CL_EPK]           ; 2. eigenes Ephemeral-Paar
    lea rdx, [rbx+CL_ESK]
    call crypto_kx_keypair

    ; 3. Antwort: SignPK | EphPK | Sig(ClientPK | EphPK)
    ; CL_CPK und CL_EPK liegen direkt hintereinander -> 64 Byte Nachricht
    lea rdi, [rbx+CL_HS]
    mov rax, [sign_pk]
    mov [rdi], rax
    mov rax, [sign_pk+8]
    mov [rdi+8], rax
    mov rax, [sign_pk+16]
    mov [rdi+16], rax
    mov rax, [sign_pk+24]
    mov [rdi+24], rax
    mov rax, [rbx+CL_EPK]
    mov [rdi+32], rax
    mov rax, [rbx+CL_EPK+8]
    mov [rdi+40], rax
    mov rax, [rbx+CL_EPK+16]
    mov [rdi+48], rax
    mov rax, [rbx+CL_EPK+24]
    mov [rdi+56], rax
    lea rcx, [rdi+64]               ; Signatur
    xor edx, edx
    lea r8, [rbx+CL_CPK]
    mov r9d, 64
    lea rax, [sign_sk]
    mov [rsp+32], rax
    call crypto_sign_detached
    mov rcx, rsi
    lea rdx, [rbx+CL_HS]
    mov r8d, HS_REPLY
    call send_all
    test eax, eax
    jz .fail

    lea rcx, [rbx+CL_RX]            ; 4. Sitzungsschluessel
    lea rdx, [rbx+CL_TX]
    lea r8, [rbx+CL_EPK]
    lea r9, [rbx+CL_ESK]
    lea rax, [rbx+CL_CPK]
    mov [rsp+32], rax
    call crypto_kx_server_session_keys
    test eax, eax
    jnz .fail
    lea rcx, [rbx+CL_URX]
    lea rdx, [rbx+CL_RX]
    call derive_udp_key
    lea rcx, [rbx+CL_UTX]
    lea rdx, [rbx+CL_TX]
    call derive_udp_key

    lea rcx, [rbx+CL_PUSH]          ; 5. secretstream in beide Richtungen
    lea rdx, [rbx+CL_HDR]
    lea r8, [rbx+CL_TX]
    call crypto_secretstream_xchacha20poly1305_init_push
    mov rcx, rsi
    lea rdx, [rbx+CL_HDR]
    mov r8d, SS_HEADER
    call send_all
    test eax, eax
    jz .fail
    mov rcx, rsi
    lea rdx, [rbx+CL_HDR]
    mov r8d, SS_HEADER
    call recv_all
    test eax, eax
    jz .fail
    lea rcx, [rbx+CL_PULL]
    lea rdx, [rbx+CL_HDR]
    lea r8, [rbx+CL_RX]
    call crypto_secretstream_xchacha20poly1305_init_pull
    test eax, eax
    jnz .fail

    lea rcx, [rbx+CL_RX]            ; nicht mehr benoetigt -> loeschen
    mov edx, CL_HS - CL_RX
    call sodium_memzero
    mov eax, 1
    jmp .ret
.fail:
    xor eax, eax
.ret:
    add rsp, 48
    pop rdi
    pop rsi
    pop rbx
    ret

; =============================================================================
; udp_thread - Sprachpakete pruefen, entschluesseln, fuer jeden Empfaenger
; neu verschluesseln und weiterleiten
; =============================================================================
udp_thread:
    sub rsp, 88
.l: mov dword [u_fromlen], 16
    mov rcx, [udp_sock]
    lea rdx, [u_in]
    mov r8d, 2048
    xor r9d, r9d
    lea rax, [u_from]
    mov [rsp+32], rax
    lea rax, [u_fromlen]
    mov [rsp+40], rax
    call recvfrom
    cmp eax, UDP_HDR + AEAD_ABYTES
    jl .l
    mov r12d, eax
    movzx eax, byte [u_in]
    cmp eax, 1
    ja .l
    movzx ebx, byte [u_in+1]
    cmp ebx, MAX_CLIENTS
    jae .l
    imul rbx, rbx, CLIENT_SIZE
    lea rax, [clients]
    add rbx, rax                    ; rbx = Absender

    lea rcx, [g_lock]
    call EnterCriticalSection
    cmp byte [rbx+CL_NAMED], 0
    je .unlock
    mov rax, [u_in+2]               ; Replay-Schutz: Zaehler muss steigen
    cmp rax, [rbx+CL_URXCTR]
    jbe .unlock
    lea rcx, [u_in]
    mov edx, r12d
    lea r8, [u_plain]
    lea r9, [rbx+CL_URX]
    call udp_open
    test rax, rax
    js .unlock
    mov r13, rax                    ; Opus-Laenge
    mov rax, [u_in+2]
    mov [rbx+CL_URXCTR], rax
    mov rax, [u_from]               ; Absenderadresse ist jetzt bestaetigt
    mov [rbx+CL_ADDR], rax
    mov rax, [u_from+8]
    mov [rbx+CL_ADDR+8], rax
    mov byte [rbx+CL_UDPOK], 1
    cmp byte [u_in], 1
    jne .unlock
    test r13, r13
    jz .unlock

    lea rsi, [clients]
    mov edi, MAX_CLIENTS
.fw:
    cmp rsi, rbx
    je .fw_next
    cmp byte [rsi+CL_NAMED], 0
    je .fw_next
    cmp byte [rsi+CL_UDPOK], 0
    je .fw_next
    mov byte [u_out], 1
    mov al, [rbx+CL_SLOT]
    mov [u_out+1], al
    inc qword [rsi+CL_UTXCTR]
    lea rcx, [u_out]
    mov rdx, [rsi+CL_UTXCTR]
    lea r8, [u_plain]
    mov r9d, r13d
    lea rax, [rsi+CL_UTX]
    mov [rsp+32], rax
    call udp_seal
    mov rcx, [udp_sock]
    lea rdx, [u_out]
    mov r8d, eax
    xor r9d, r9d
    lea rax, [rsi+CL_ADDR]
    mov [rsp+32], rax
    mov qword [rsp+40], 16
    call sendto
.fw_next:
    add rsi, CLIENT_SIZE
    dec edi
    jnz .fw
.unlock:
    lea rcx, [g_lock]
    call LeaveCriticalSection
    jmp .l
