; =============================================================================
; proto.inc - Netzwerk- und Krypto-Grundlagen (libsodium), von Server und
; Client gemeinsam benutzt.
;
; Verbindungsaufbau (TCP):
;   C -> S : Client-Ephemeral-Schluessel (X25519, 32 Byte)
;   S -> C : Server-Signaturschluessel (Ed25519, 32) | Server-Ephemeral (32)
;            | Signatur ueber (Client-Eph | Server-Eph) (64)
;   beide  : crypto_kx -> Sitzungsschluessel rx/tx (Forward Secrecy)
;            UDP-Schluessel = BLAKE2b(Label, key = rx bzw. tx)
;   S -> C : secretstream-Header (24), C -> S : secretstream-Header (24)
;
; TCP-Nachrichten: [Laenge u16][secretstream-Chiffretext]
;   Klartext: [Typ][Daten]   Typen: N=Name M=Text W=Willkommen U=Liste
; UDP-Pakete:  [Typ][Slot][Zaehler u64][AEAD-Chiffretext]   (10 Byte Kopf = AD)
;   Typ 0 = Lebenszeichen, Typ 1 = Opus-Sprachpaket
; =============================================================================

extern send, recv
extern sodium_init, sodium_memzero
extern crypto_kx_keypair, crypto_kx_client_session_keys, crypto_kx_server_session_keys
extern crypto_sign_keypair, crypto_sign_detached, crypto_sign_verify_detached
extern crypto_sign_ed25519_sk_to_pk
extern crypto_generichash
extern crypto_secretstream_xchacha20poly1305_init_push
extern crypto_secretstream_xchacha20poly1305_init_pull
extern crypto_secretstream_xchacha20poly1305_push
extern crypto_secretstream_xchacha20poly1305_pull
extern crypto_aead_chacha20poly1305_ietf_encrypt
extern crypto_aead_chacha20poly1305_ietf_decrypt

%define KEY_BYTES    32
%define SIGN_SK      64
%define SIG_BYTES    64
%define SS_HEADER    24
%define SS_ABYTES    17
%define SS_STATE     64
%define AEAD_ABYTES  16
%define MAX_PLAIN    1200
%define MAX_CT       (MAX_PLAIN + SS_ABYTES)
%define UDP_HDR      10
%define HS_REPLY     128
%define MAX_SLOTS    32

%define MSG_NAME     'N'
%define MSG_TEXT     'M'
%define MSG_WELCOME  'W'
%define MSG_USERS    'U'

section .data
udp_label       db "ASMSPEAK-UDP-v1"
udp_label_len   equ $ - udp_label
hexchars        db "0123456789ABCDEF"

section .text

; -----------------------------------------------------------------------------
; send_all / recv_all - rcx = Socket, rdx = Puffer, r8d = Laenge -> eax 1/0
; -----------------------------------------------------------------------------
send_all:
    push rbx
    push rsi
    push rdi
    sub rsp, 32
    mov rbx, rcx
    mov rsi, rdx
    mov edi, r8d
.l: test edi, edi
    jz .ok
    mov rcx, rbx
    mov rdx, rsi
    mov r8d, edi
    xor r9d, r9d
    call send
    cmp eax, 0
    jle .fail
    mov eax, eax
    add rsi, rax
    sub edi, eax
    jmp .l
.ok:
    mov eax, 1
    jmp .ret
.fail:
    xor eax, eax
.ret:
    add rsp, 32
    pop rdi
    pop rsi
    pop rbx
    ret

recv_all:
    push rbx
    push rsi
    push rdi
    sub rsp, 32
    mov rbx, rcx
    mov rsi, rdx
    mov edi, r8d
.l: test edi, edi
    jz .ok
    mov rcx, rbx
    mov rdx, rsi
    mov r8d, edi
    xor r9d, r9d
    call recv
    cmp eax, 0
    jle .fail
    mov eax, eax
    add rsi, rax
    sub edi, eax
    jmp .l
.ok:
    mov eax, 1
    jmp .ret
.fail:
    xor eax, eax
.ret:
    add rsp, 32
    pop rdi
    pop rsi
    pop rbx
    ret

; -----------------------------------------------------------------------------
; ss_send - verschluesselte Nachricht senden
;   rcx = Socket, rdx = Push-Zustand, r8 = Klartext, r9d = Laenge -> eax 1/0
; -----------------------------------------------------------------------------
ss_send:
    push rbx
    push rsi
    push rdi
    sub rsp, 1392                   ; [rsp+64] clen, [rsp+80] Rahmen
    mov rbx, rcx
    cmp r9d, MAX_PLAIN
    ja .fail
    mov rcx, rdx                    ; state
    lea rdx, [rsp+82]               ; Chiffretext hinter 2-Byte-Laenge
    mov rax, r8
    lea r8, [rsp+64]                ; &clen
    mov [rsp+32], r9                ; mlen
    mov r9, rax                     ; m
    mov qword [rsp+40], 0           ; ad
    mov qword [rsp+48], 0           ; adlen
    mov qword [rsp+56], 0           ; tag = MESSAGE
    call crypto_secretstream_xchacha20poly1305_push
    test eax, eax
    jnz .fail
    mov eax, [rsp+64]
    mov [rsp+80], ax
    lea r8d, [eax+2]
    mov rcx, rbx
    lea rdx, [rsp+80]
    call send_all
    jmp .ret
.fail:
    xor eax, eax
.ret:
    add rsp, 1392
    pop rdi
    pop rsi
    pop rbx
    ret

; -----------------------------------------------------------------------------
; ss_recv - verschluesselte Nachricht empfangen und pruefen
;   rcx = Socket, rdx = Pull-Zustand, r8 = Ziel (MAX_PLAIN+1)
;   -> rax = Klartextlaenge (nullterminiert) oder -1 bei Fehler/Manipulation
; -----------------------------------------------------------------------------
ss_recv:
    push rbx
    push rsi
    push rdi
    push r12
    sub rsp, 1400                   ; [rsp+72] mlen [rsp+80] tag [rsp+96] len [rsp+112] ct
    mov rbx, rcx
    mov rsi, rdx
    mov rdi, r8
    mov rcx, rbx
    lea rdx, [rsp+96]
    mov r8d, 2
    call recv_all
    test eax, eax
    jz .fail
    movzx r12d, word [rsp+96]
    cmp r12d, SS_ABYTES
    jb .fail
    cmp r12d, MAX_CT
    ja .fail
    mov rcx, rbx
    lea rdx, [rsp+112]
    mov r8d, r12d
    call recv_all
    test eax, eax
    jz .fail
    mov rcx, rsi
    mov rdx, rdi
    lea r8, [rsp+72]
    lea r9, [rsp+80]
    lea rax, [rsp+112]
    mov [rsp+32], rax
    mov [rsp+40], r12
    mov qword [rsp+48], 0
    mov qword [rsp+56], 0
    call crypto_secretstream_xchacha20poly1305_pull
    test eax, eax
    jnz .fail
    mov rax, [rsp+72]
    mov byte [rdi+rax], 0
    jmp .ret
.fail:
    mov rax, -1
.ret:
    add rsp, 1400
    pop r12
    pop rdi
    pop rsi
    pop rbx
    ret

; -----------------------------------------------------------------------------
; derive_udp_key - rcx = Ziel (32), rdx = Sitzungsschluessel (32)
; -----------------------------------------------------------------------------
derive_udp_key:
    sub rsp, 56
    mov [rsp+32], rdx
    mov qword [rsp+40], KEY_BYTES
    mov edx, KEY_BYTES
    lea r8, [udp_label]
    mov r9d, udp_label_len
    call crypto_generichash
    add rsp, 56
    ret

; -----------------------------------------------------------------------------
; fingerprint - rcx = oeffentlicher Schluessel, rdx = Ziel "XXXX-XXXX-XXXX-XXXX"
; -----------------------------------------------------------------------------
fingerprint:
    push rbx
    push rsi
    sub rsp, 72
    mov rsi, rdx
    mov r8, rcx
    lea rcx, [rsp+48]
    mov edx, 16
    mov r9d, KEY_BYTES
    mov qword [rsp+32], 0
    mov qword [rsp+40], 0
    call crypto_generichash
    lea r8, [hexchars]
    xor ebx, ebx
.l: movzx eax, byte [rsp+48+rbx]
    mov ecx, eax
    shr ecx, 4
    mov cl, [r8+rcx]
    mov [rsi], cl
    and eax, 15
    mov al, [r8+rax]
    mov [rsi+1], al
    add rsi, 2
    inc ebx
    cmp ebx, 8
    je .d
    test ebx, 1
    jnz .l
    mov byte [rsi], '-'
    inc rsi
    jmp .l
.d: mov byte [rsi], 0
    add rsp, 72
    pop rsi
    pop rbx
    ret

; -----------------------------------------------------------------------------
; udp_seal - verschluesselt ein UDP-Paket
;   rcx = Paketpuffer (Kopf [Typ][Slot] schon gesetzt), rdx = Zaehler,
;   r8 = Klartext, r9d = Laenge, [rsp+40] = Schluessel  -> eax = Paketlaenge
; -----------------------------------------------------------------------------
udp_seal:
    push rbx
    sub rsp, 112                    ; [rsp+80] clen  [rsp+96] Nonce
    mov rbx, rcx
    mov rax, [rsp+112+8+40]         ; Schluessel (5. Parameter)
    mov [rcx+2], rdx                ; Zaehler in den Kopf
    mov [rsp+96], rdx               ; Nonce = Zaehler | 0
    mov dword [rsp+104], 0
    mov [rsp+64], rax               ; k
    lea rax, [rsp+96]
    mov [rsp+56], rax               ; npub
    mov qword [rsp+48], 0           ; nsec
    mov qword [rsp+40], UDP_HDR     ; adlen
    mov [rsp+32], rcx               ; ad = Kopf
    lea rcx, [rbx+UDP_HDR]          ; c
    lea rdx, [rsp+80]               ; &clen
    mov r9d, r9d                    ; mlen, r8 = m
    call crypto_aead_chacha20poly1305_ietf_encrypt
    mov eax, [rsp+80]
    add eax, UDP_HDR
    add rsp, 112
    pop rbx
    ret

; -----------------------------------------------------------------------------
; udp_open - prueft und entschluesselt ein UDP-Paket
;   rcx = Paket, edx = Paketlaenge, r8 = Ziel, r9 = Schluessel
;   -> rax = Klartextlaenge oder -1
; -----------------------------------------------------------------------------
udp_open:
    push rbx
    sub rsp, 112
    cmp edx, UDP_HDR + AEAD_ABYTES
    jb .fail
    mov rbx, rcx
    mov rax, [rcx+2]
    mov [rsp+96], rax
    mov dword [rsp+104], 0
    mov [rsp+64], r9                ; k
    lea rax, [rsp+96]
    mov [rsp+56], rax               ; npub
    mov qword [rsp+48], UDP_HDR     ; adlen
    mov [rsp+40], rcx               ; ad
    sub edx, UDP_HDR
    mov [rsp+32], rdx               ; clen
    lea r9, [rcx+UDP_HDR]           ; c
    mov rcx, r8                     ; m
    lea rdx, [rsp+80]               ; &mlen
    xor r8d, r8d                    ; nsec
    call crypto_aead_chacha20poly1305_ietf_decrypt
    test eax, eax
    jnz .fail
    mov rax, [rsp+80]
    jmp .ret
.fail:
    mov rax, -1
.ret:
    add rsp, 112
    pop rbx
    ret
