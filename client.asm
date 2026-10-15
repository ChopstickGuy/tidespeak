; =============================================================================
; ASM-Speak Client v2 - Fenster-App fuer verschluesselten Text- und Sprachchat
; Windows x64, NASM, Win32-GUI, Opus, libsodium
;
; Threads:
;   GUI-Thread     - Fenster, Eingaben, Anzeige (alle Fensterzugriffe nur hier)
;   net_thread     - Verbindungsaufbau, empfaengt Text/Teilnehmerliste
;                    (meldet sich per SendMessage WM_APP_* beim GUI-Thread)
;   udp_thread     - empfaengt Sprache, entschluesselt, Opus-Decoder, Mischer
;   play_thread    - Ringpuffer -> Lautsprecher (waveOut)
;   capture_thread - Mikrofon (waveIn) -> Opus-Encoder -> verschluesselt senden
;
; Audio: 48 kHz mono, 20-ms-Rahmen (960 Samples), Opus VOIP 32 kbit/s
; =============================================================================

%include "common.inc"
%include "proto.inc"

%define FRAME_SAMPLES   960
%define FRAME_BYTES     1920
%define NBUF            4
%define WAVEHDR_SIZE    48
%define WHDR_DONE       1
%define RING_SAMPLES    32768
%define RING_MASK       (RING_SAMPLES - 1)
%define JITTER_SAMPLES  (2 * FRAME_SAMPLES)
%define OPUS_MAX        400
%define WAVE_MAPPER     0xFFFFFFFF
%define CALLBACK_EVENT  0x50000
%define VK_F2           0x71
%define KEEPALIVE_MS    2000
%define SPEAK_HOLD_MS   350
%define NAME_ENTRY      40
%define CP_UTF8         65001

; Fenster-Nachrichten
%define WM_CREATE       0x0001
%define WM_DESTROY      0x0002
%define WM_SIZE         0x0005
%define WM_SETFONT      0x0030
%define WM_GETMINMAXINFO 0x0024
%define WM_COMMAND      0x0111
%define WM_TIMER        0x0113
%define WM_VSCROLL      0x0115
%define WM_CTLCOLORSTATIC 0x0138
%define WM_APP_TEXT     0x8001
%define WM_APP_USERS    0x8002
%define WM_APP_STATE    0x8003
%define EM_SETSEL       0x00B1
%define EM_REPLACESEL   0x00C2
%define EM_SETLIMITTEXT 0x00C5
%define EM_SETCUEBANNER 0x1501
%define LB_ADDSTRING    0x0180
%define LB_RESETCONTENT 0x0184
%define BM_GETCHECK     0x00F0
%define SB_BOTTOM       7

; Stile
%define WS_OVERLAPPEDWINDOW 0x00CF0000
%define WS_CLIPCHILDREN 0x02000000
%define WS_CHILD        0x40000000
%define WS_VISIBLE      0x10000000
%define WS_TABSTOP      0x00010000
%define WS_VSCROLL      0x00200000
%define WS_EX_CLIENTEDGE 0x00000200
%define ES_MULTILINE    0x0004
%define ES_AUTOVSCROLL  0x0040
%define ES_AUTOHSCROLL  0x0080
%define ES_READONLY     0x0800
%define SS_RIGHT        0x0002
%define SS_CENTERIMAGE  0x0200
%define BS_AUTOCHECKBOX 0x0003
%define LBS_NOINTEGRALHEIGHT 0x0100
%define LBS_NOSEL       0x4000
%define CW_USEDEFAULT   0x80000000

; Control-IDs
%define IDOK            1
%define ID_IP           101
%define ID_NAME         102
%define ID_CONNECT      103
%define ID_HISTORY      104
%define ID_USERS        105
%define ID_INPUT        106
%define ID_SEND         107
%define ID_MUTE         108
%define ID_OPEN         109
%define ID_STATUS       110

%define SOL_SOCKET      0xFFFF
%define SO_RCVTIMEO     0x1006
%define TCP_NODELAY     1

extern GetModuleHandleW, GetModuleFileNameW, GetEnvironmentVariableW
extern GetPrivateProfileStringW, WritePrivateProfileStringW
extern CreateThread, CloseHandle, CreateEventA, WaitForSingleObject, Sleep
extern GetTickCount64, GetLocalTime
extern InitializeCriticalSection, EnterCriticalSection, LeaveCriticalSection
extern MultiByteToWideChar, WideCharToMultiByte
extern RegisterClassExW, CreateWindowExW, DefWindowProcW, ShowWindow, UpdateWindow
extern GetMessageW, TranslateMessage, DispatchMessageW, IsDialogMessageW
extern PostQuitMessage, SendMessageW, SetWindowTextW, GetWindowTextW
extern GetWindowTextLengthW, MoveWindow, EnableWindow, SetFocus, GetFocus
extern SetTimer, LoadCursorW, LoadIconW, MessageBoxW, GetAsyncKeyState
extern CreateFontW, GetStockObject, SetBkColor
extern WSAStartup, socket, connect, closesocket, setsockopt, inet_addr, gethostbyname
extern waveInOpen, waveInPrepareHeader, waveInAddBuffer, waveInStart
extern waveOutOpen, waveOutPrepareHeader, waveOutWrite
extern opus_encoder_create, opus_encoder_ctl, opus_encode
extern opus_decoder_create, opus_decode

; -----------------------------------------------------------------------------
section .data
w_class         dw __utf16__("ASMSpeakWnd"), 0
w_title         dw __utf16__("ASM-Speak"), 0
w_font          dw __utf16__("Segoe UI"), 0
w_edit          dw __utf16__("EDIT"), 0
w_button        dw __utf16__("BUTTON"), 0
w_static        dw __utf16__("STATIC"), 0
w_listbox       dw __utf16__("LISTBOX"), 0
w_empty         dw 0
w_lbl_ip        dw __utf16__("Server:"), 0
w_lbl_name      dw __utf16__("Name:"), 0
w_lbl_users     dw __utf16__("Im Channel"), 0
w_connect       dw __utf16__("Verbinden"), 0
w_cancel        dw __utf16__("Abbrechen"), 0
w_disconnect    dw __utf16__("Trennen"), 0
w_send          dw __utf16__("Senden"), 0
w_mute          dw __utf16__("Mikrofon stumm"), 0
w_open          dw __utf16__("Dauersenden (sonst F2 halten)"), 0
w_cue           dw __utf16__("Nachricht eingeben und Enter drücken…"), 0
w_st_off        dw __utf16__("Nicht verbunden"), 0
w_st_conn       dw __utf16__("Verbinde…"), 0
w_st_ptt        dw __utf16__("Verbunden  –  zum Sprechen F2 gedrückt halten"), 0
w_st_open       dw __utf16__("Verbunden  –  Mikrofon offen"), 0
w_st_muted      dw __utf16__("Verbunden  –  Mikrofon stumm"), 0
w_st_talk       dw __utf16__("●  SENDET"), 0
w_talk          dw __utf16__("●  "), 0
w_idle          dw __utf16__("○  "), 0
w_you           dw __utf16__("  (du)"), 0
w_section       dw __utf16__("ASM-Speak"), 0
w_key_server    dw __utf16__("Server"), 0
w_key_name      dw __utf16__("Name"), 0
w_default_ip    dw __utf16__("127.0.0.1"), 0
w_username      dw __utf16__("USERNAME"), 0
w_guest         dw __utf16__("Gast"), 0
w_ini_name      dw __utf16__("asmspeak.ini"), 0
w_err_title     dw __utf16__("ASM-Speak - Fehler"), 0
w_err_sodium    dw __utf16__("libsodium konnte nicht gestartet werden."), 0

t_welcome       db "Willkommen bei ASM-Speak! Server und Namen eintragen, dann Verbinden.",0
t_no_in         db "Hinweis: Kein Mikrofon gefunden - nur Zuhören und Text.",0
t_no_out        db "Hinweis: Keine Audioausgabe gefunden.",0
t_connecting    db "Verbinde mit ",0
t_dots          db " …",0
t_err_resolve   db "Server-Adresse nicht gefunden.",0
t_err_connect   db "Server nicht erreichbar (läuft server.exe? Firewall?).",0
t_err_hs        db "Sicherer Verbindungsaufbau fehlgeschlagen - falscher oder manipulierter Server!",0
t_secure        db "Sicher verbunden (X25519 + XChaCha20-Poly1305). Server-Fingerabdruck: ",0
t_lost          db "Verbindung getrennt.",0
t_default_ip    db "127.0.0.1",0
t_guest         db "Gast",0

wave_fmt        dw 1                ; PCM
                dw 1                ; mono
                dd 48000
                dd 96000
                dw 2
                dw 16
                dw 0

; -----------------------------------------------------------------------------
section .bss
alignb 16
hinst       resq 1
hwnd_main   resq 1
hfont       resq 1
h_lbl_ip    resq 1
h_ip        resq 1
h_lbl_name  resq 1
h_name      resq 1
h_connect   resq 1
h_history   resq 1
h_lbl_users resq 1
h_users     resq 1
h_input     resq 1
h_send      resq 1
h_mute      resq 1
h_open      resq 1
h_status    resq 1
g_w         resd 1
g_h         resd 1
shown_mask  resd 1
shown_key   resd 1
g_state     resb 1
voice_ready resb 1
my_slot     resb 1
muted       resb 1
open_mic    resb 1
talking     resb 1
audio_in_ok resb 1
audio_out_ok resb 1
alignb 16
g_tcp       resq 1
g_udp       resq 1
srv_addr    resb 16
wsadata     resb 512
systime     resb 16
ini_path    resw 300
g_wtmp      resw 2048
g_wtmp2     resw 256
chat_buf    resb 1300
conn_ip     resb 128
name_msg    resb 160
net_msg     resb 512
fp_text     resb 32
users_present resb MAX_SLOTS
users_name  resb NAME_ENTRY * MAX_SLOTS
alignb 16
; Krypto
c_pk        resb 32
c_sk        resb 32
hs_buf      resb HS_REPLY
hs_msg      resb 64
k_rx        resb 32
k_tx        resb 32
udp_rx_key  resb 32
udp_tx_key  resb 32
push_state  resb SS_STATE
pull_state  resb SS_STATE
ss_hdr      resb SS_HEADER + 8
udp_tx_ctr  resq 1
udp_rx_ctr  resq 1
net_rbuf    resb MAX_PLAIN + 16
; Audio
alignb 16
hwi         resq 1
hwo         resq 1
ev_in       resq 1
ev_out      resq 1
encoder     resq 1
decoders    resq MAX_SLOTS
last_spoke  resq MAX_SLOTS
cursors     resq MAX_SLOTS
playpos     resq 1
last_send   resq 1
crit        resb 64
opus_err    resd 2
alignb 16
in_hdrs     resb WAVEHDR_SIZE * NBUF
out_hdrs    resb WAVEHDR_SIZE * NBUF
in_bufs     resb FRAME_BYTES * NBUF
out_bufs    resb FRAME_BYTES * NBUF
pcm_dec     resb FRAME_BYTES * 2
opus_out    resb OPUS_MAX
c_out       resb OPUS_MAX + 64
u_in        resb 2048
u_plain     resb 2048
ring        resw RING_SAMPLES

; -----------------------------------------------------------------------------
section .text
global start

start:
    sub rsp, 200                    ; MSG bzw. WNDCLASSEXW bei [rsp+112]
    xor ecx, ecx
    call GetModuleHandleW
    mov [hinst], rax

    call sodium_init
    test eax, eax
    jns .sodium_ok
    xor ecx, ecx
    lea rdx, [w_err_sodium]
    lea r8, [w_err_title]
    mov r9d, 0x10
    call MessageBoxW
    mov ecx, 1
    call ExitProcess
.sodium_ok:
    mov ecx, 0x0202
    lea rdx, [wsadata]
    call WSAStartup

    ; INI-Datei liegt neben der EXE
    xor ecx, ecx
    lea rdx, [ini_path]
    mov r8d, 260
    call GetModuleFileNameW
    lea rcx, [ini_path]
    lea rdx, [rcx+rax*2]
.slash:
    cmp rdx, rcx
    jbe .slash_d
    cmp word [rdx-2], '\'
    je .slash_d
    sub rdx, 2
    jmp .slash
.slash_d:
    mov rcx, rdx
    lea rdx, [w_ini_name]
    call wcat

    call audio_init

    ; Fensterklasse
    lea rbx, [rsp+112]              ; WNDCLASSEXW (80 Byte) voruebergehend hier
    mov dword [rbx], 80
    mov dword [rbx+4], 3            ; CS_HREDRAW | CS_VREDRAW
    lea rax, [wnd_proc]
    mov [rbx+8], rax
    mov qword [rbx+16], 0
    mov rax, [hinst]
    mov [rbx+24], rax
    xor ecx, ecx
    mov edx, 32512                  ; IDI_APPLICATION
    call LoadIconW
    mov [rbx+32], rax
    mov [rbx+72], rax
    xor ecx, ecx
    mov edx, 32512                  ; IDC_ARROW
    call LoadCursorW
    mov [rbx+40], rax
    mov qword [rbx+48], 16          ; COLOR_BTNFACE + 1
    mov qword [rbx+56], 0
    lea rax, [w_class]
    mov [rbx+64], rax
    mov rcx, rbx
    call RegisterClassExW

    mov ecx, 0
    lea rdx, [w_class]
    lea r8, [w_title]
    mov r9d, WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN
    mov dword [rsp+32], CW_USEDEFAULT
    mov dword [rsp+36], 0
    mov dword [rsp+40], CW_USEDEFAULT
    mov dword [rsp+44], 0
    mov qword [rsp+48], 860
    mov qword [rsp+56], 580
    mov qword [rsp+64], 0
    mov qword [rsp+72], 0
    mov rax, [hinst]
    mov [rsp+80], rax
    mov qword [rsp+88], 0
    call CreateWindowExW
    mov [hwnd_main], rax
    mov rcx, rax
    mov edx, 5                      ; SW_SHOW
    call ShowWindow
    mov rcx, [hwnd_main]
    call UpdateWindow
    mov rcx, [hwnd_main]
    mov edx, 1
    mov r8d, 120
    xor r9d, r9d
    call SetTimer

.msgloop:
    lea rcx, [rsp+112]
    xor edx, edx
    xor r8d, r8d
    xor r9d, r9d
    call GetMessageW
    test eax, eax
    jz .quit
    cmp eax, -1
    je .quit
    mov rcx, [hwnd_main]            ; Tab/Enter wie in einem Dialog
    lea rdx, [rsp+112]
    call IsDialogMessageW
    test eax, eax
    jnz .msgloop
    lea rcx, [rsp+112]
    call TranslateMessage
    lea rcx, [rsp+112]
    call DispatchMessageW
    jmp .msgloop
.quit:
    xor ecx, ecx
    call ExitProcess

; =============================================================================
; Fensterprozedur
; =============================================================================
wnd_proc:
    push rbx
    push rsi
    push rdi
    push r12
    push r13
    sub rsp, 112
    mov rbx, rcx
    mov esi, edx
    mov rdi, r8
    mov r12, r9

    cmp esi, WM_CREATE
    je .create
    cmp esi, WM_SIZE
    je .size
    cmp esi, WM_COMMAND
    je .command
    cmp esi, WM_TIMER
    je .timer
    cmp esi, WM_APP_TEXT
    je .app_text
    cmp esi, WM_APP_USERS
    je .app_users
    cmp esi, WM_APP_STATE
    je .app_state
    cmp esi, WM_CTLCOLORSTATIC
    je .ctlcolor
    cmp esi, WM_GETMINMAXINFO
    je .minmax
    cmp esi, WM_DESTROY
    je .destroy
.default:
    mov rcx, rbx
    mov edx, esi
    mov r8, rdi
    mov r9, r12
    call DefWindowProcW
    jmp .ret

.create:
    mov [hwnd_main], rbx
    mov ecx, -15                    ; Segoe UI, ca. 11 pt
    xor edx, edx
    xor r8d, r8d
    xor r9d, r9d
    mov qword [rsp+32], 400
    mov qword [rsp+40], 0
    mov qword [rsp+48], 0
    mov qword [rsp+56], 0
    mov qword [rsp+64], 1
    mov qword [rsp+72], 0
    mov qword [rsp+80], 0
    mov qword [rsp+88], 5           ; CLEARTYPE_QUALITY
    mov qword [rsp+96], 0
    lea rax, [w_font]
    mov [rsp+104], rax
    call CreateFontW
    mov [hfont], rax
    call create_controls
    xor ecx, ecx
    call set_state
    lea rcx, [t_welcome]
    call append_history
    cmp byte [audio_in_ok], 0
    jne .c_in
    lea rcx, [t_no_in]
    call append_history
.c_in:
    cmp byte [audio_out_ok], 0
    jne .c_out
    lea rcx, [t_no_out]
    call append_history
.c_out:
    jmp .zero

.size:
    movzx eax, r12w
    mov [g_w], eax
    mov eax, r12d
    shr eax, 16
    mov [g_h], eax
    call layout
    jmp .zero

.command:
    movzx eax, di
    cmp eax, ID_CONNECT
    je .cmd_connect
    cmp eax, ID_SEND
    je .cmd_send
    cmp eax, IDOK
    je .cmd_ok
    cmp eax, ID_MUTE
    je .cmd_mute
    cmp eax, ID_OPEN
    je .cmd_open
    jmp .zero
.cmd_ok:                            ; Enter: im Eingabefeld senden, sonst verbinden
    call GetFocus
    cmp rax, [h_input]
    je .cmd_send
    cmp byte [g_state], 0
    jne .zero
.cmd_connect:
    call toggle_connect
    jmp .zero
.cmd_send:
    call send_chat
    jmp .zero
.cmd_mute:
    mov rcx, [h_mute]
    mov edx, BM_GETCHECK
    xor r8d, r8d
    xor r9d, r9d
    call SendMessageW
    mov [muted], al
    jmp .zero
.cmd_open:
    mov rcx, [h_open]
    mov edx, BM_GETCHECK
    xor r8d, r8d
    xor r9d, r9d
    call SendMessageW
    mov [open_mic], al
    jmp .zero

.timer:
    call timer_tick
    jmp .zero

.app_text:
    mov rcx, r12
    call append_history
    jmp .zero

.app_users:
    mov rcx, r12
    mov edx, edi
    call parse_users
    mov dword [shown_mask], 0
    call rebuild_users
    jmp .zero

.app_state:
    mov ecx, edi
    call set_state
    jmp .zero

.ctlcolor:                          ; Chatverlauf weiss statt grau
    cmp r12, [h_history]
    jne .default
    mov rcx, rdi
    mov edx, 0xFFFFFF
    call SetBkColor
    xor ecx, ecx                    ; WHITE_BRUSH
    call GetStockObject
    jmp .ret

.minmax:
    mov dword [r12+24], 680         ; ptMinTrackSize
    mov dword [r12+28], 420
    jmp .zero

.destroy:
    xor ecx, ecx
    call PostQuitMessage
.zero:
    xor eax, eax
.ret:
    add rsp, 112
    pop r13
    pop r12
    pop rdi
    pop rsi
    pop rbx
    ret

; -----------------------------------------------------------------------------
; make_ctrl - rcx = Klasse, rdx = Text, r8d = Stil, r9d = ID, [5.] = ExStil
; -----------------------------------------------------------------------------
make_ctrl:
    push rbx
    sub rsp, 96
    mov rax, [rsp+144]              ; ExStil
    mov r10, rcx
    mov r11, rdx
    mov ecx, eax
    mov rdx, r10
    mov qword [rsp+88], 0
    mov rax, [hinst]
    mov [rsp+80], rax
    mov eax, r9d
    mov [rsp+72], rax               ; ID
    mov rax, [hwnd_main]
    mov [rsp+64], rax
    mov qword [rsp+56], 10
    mov qword [rsp+48], 10
    mov qword [rsp+40], 0
    mov qword [rsp+32], 0
    or r8d, WS_CHILD | WS_VISIBLE
    mov r9d, r8d
    mov r8, r11
    call CreateWindowExW
    mov rbx, rax
    mov rcx, rax
    mov edx, WM_SETFONT
    mov r8, [hfont]
    mov r9d, 1
    call SendMessageW
    mov rax, rbx
    add rsp, 96
    pop rbx
    ret

%macro CTRL 6       ; Ziel, Klasse, Text, Stil, ID, ExStil
    lea rcx, [%2]
    lea rdx, [%3]
    mov r8d, %4
    mov r9d, %5
    mov qword [rsp+32], %6
    call make_ctrl
    mov [%1], rax
%endmacro

%macro SENDMSG 4    ; Fenster(Variable), Nachricht, wParam, lParam
    mov rcx, [%1]
    mov edx, %2
    mov r8, %3
    mov r9, %4
    call SendMessageW
%endmacro

create_controls:
    push rbx
    sub rsp, 64
    CTRL h_lbl_ip,    w_static,  w_lbl_ip,    SS_RIGHT|SS_CENTERIMAGE, 0, 0
    CTRL h_ip,        w_edit,    w_empty,     WS_TABSTOP|ES_AUTOHSCROLL, ID_IP, WS_EX_CLIENTEDGE
    CTRL h_lbl_name,  w_static,  w_lbl_name,  SS_RIGHT|SS_CENTERIMAGE, 0, 0
    CTRL h_name,      w_edit,    w_empty,     WS_TABSTOP|ES_AUTOHSCROLL, ID_NAME, WS_EX_CLIENTEDGE
    CTRL h_connect,   w_button,  w_connect,   WS_TABSTOP, ID_CONNECT, 0
    CTRL h_history,   w_edit,    w_empty,     ES_MULTILINE|ES_AUTOVSCROLL|ES_READONLY|WS_VSCROLL, ID_HISTORY, WS_EX_CLIENTEDGE
    CTRL h_lbl_users, w_static,  w_lbl_users, SS_CENTERIMAGE, 0, 0
    CTRL h_users,     w_listbox, w_empty,     LBS_NOINTEGRALHEIGHT|LBS_NOSEL|WS_VSCROLL, ID_USERS, WS_EX_CLIENTEDGE
    CTRL h_input,     w_edit,    w_empty,     WS_TABSTOP|ES_AUTOHSCROLL, ID_INPUT, WS_EX_CLIENTEDGE
    CTRL h_send,      w_button,  w_send,      WS_TABSTOP, ID_SEND, 0
    CTRL h_mute,      w_button,  w_mute,      WS_TABSTOP|BS_AUTOCHECKBOX, ID_MUTE, 0
    CTRL h_open,      w_button,  w_open,      WS_TABSTOP|BS_AUTOCHECKBOX, ID_OPEN, 0
    CTRL h_status,    w_static,  w_empty,     SS_CENTERIMAGE, ID_STATUS, 0

    SENDMSG h_history, EM_SETLIMITTEXT, 0, 0
    SENDMSG h_input, EM_SETLIMITTEXT, 300, 0
    lea r9, [w_cue]
    SENDMSG h_input, EM_SETCUEBANNER, 1, r9
    SENDMSG h_name, EM_SETLIMITTEXT, 24, 0

    ; Vorgaben: Windows-Benutzername, dann gespeicherte Werte aus der INI
    lea rcx, [w_username]
    lea rdx, [g_wtmp2]
    mov r8d, 40
    call GetEnvironmentVariableW
    test eax, eax
    jz .no_user
    cmp eax, 40
    jb .have_user
.no_user:
    lea rcx, [g_wtmp2]
    lea rdx, [w_guest]
    call wcat
.have_user:
    lea rcx, [w_section]
    lea rdx, [w_key_server]
    lea r8, [w_default_ip]
    lea r9, [g_wtmp]
    mov qword [rsp+32], 128
    lea rax, [ini_path]
    mov [rsp+40], rax
    call GetPrivateProfileStringW
    mov rcx, [h_ip]
    lea rdx, [g_wtmp]
    call SetWindowTextW
    lea rcx, [w_section]
    lea rdx, [w_key_name]
    lea r8, [g_wtmp2]
    lea r9, [g_wtmp]
    mov qword [rsp+32], 40
    lea rax, [ini_path]
    mov [rsp+40], rax
    call GetPrivateProfileStringW
    mov rcx, [h_name]
    lea rdx, [g_wtmp]
    call SetWindowTextW
    add rsp, 64
    pop rbx
    ret

; -----------------------------------------------------------------------------
; layout - ordnet alle Controls nach der Fenstergroesse an
; -----------------------------------------------------------------------------
%macro PLACE 9      ; Fenster, x-Basis,x, y-Basis,y, b-Basis,b, h-Basis,h
    mov rcx, [%1]
    lea edx, [%2 + %3]
    lea r8d, [%4 + %5]
    lea r9d, [%6 + %7]
    lea eax, [%8 + %9]
    mov [rsp+32], rax
    call place
%endmacro

layout:
    push rbx
    push rsi
    push r12
    sub rsp, 48
    mov ebx, [g_w]                  ; rbx = Breite, rsi = Hoehe, r12 = 0
    mov esi, [g_h]
    xor r12d, r12d
    PLACE h_lbl_ip,    r12, 8,     r12, 12,   r12, 56,    r12, 26
    PLACE h_ip,        r12, 70,    r12, 12,   r12, 170,   r12, 26
    PLACE h_lbl_name,  r12, 248,   r12, 12,   r12, 48,    r12, 26
    PLACE h_name,      r12, 302,   r12, 12,   r12, 170,   r12, 26
    PLACE h_connect,   r12, 486,   r12, 11,   r12, 130,   r12, 28
    PLACE h_history,   r12, 12,    r12, 50,   rbx, -236,  rsi, -136
    PLACE h_lbl_users, rbx, -212,  r12, 48,   r12, 200,   r12, 22
    PLACE h_users,     rbx, -212,  r12, 72,   r12, 200,   rsi, -158
    PLACE h_input,     r12, 12,    rsi, -78,  rbx, -148,  r12, 28
    PLACE h_send,      rbx, -124,  rsi, -79,  r12, 112,   r12, 30
    PLACE h_mute,      r12, 14,    rsi, -38,  r12, 150,   r12, 24
    PLACE h_open,      r12, 172,   rsi, -38,  r12, 250,   r12, 24
    PLACE h_status,    r12, 440,   rsi, -38,  rbx, -452,  r12, 24
    add rsp, 48
    pop r12
    pop rsi
    pop rbx
    ret

; place - rcx = Fenster, edx = x, r8d = y, r9d = Breite, [5.] = Hoehe
place:
    sub rsp, 56
    mov eax, [rsp+96]
    mov [rsp+32], rax
    mov qword [rsp+40], 1
    call MoveWindow
    add rsp, 56
    ret

; -----------------------------------------------------------------------------
; set_state - ecx = 0 getrennt, 1 verbinde, 2 verbunden
; -----------------------------------------------------------------------------
set_state:
    push rbx
    sub rsp, 32
    mov ebx, ecx
    mov [g_state], cl
    lea rdx, [w_connect]
    cmp ebx, 1
    jne .s1
    lea rdx, [w_cancel]
.s1:
    cmp ebx, 2
    jne .s2
    lea rdx, [w_disconnect]
.s2:
    mov rcx, [h_connect]
    call SetWindowTextW
    xor edx, edx
    test ebx, ebx
    sete dl
    mov rcx, [h_ip]
    call EnableWindow
    xor edx, edx
    test ebx, ebx
    sete dl
    mov rcx, [h_name]
    call EnableWindow
    xor edx, edx
    cmp ebx, 2
    sete dl
    mov rcx, [h_input]
    call EnableWindow
    xor edx, edx
    cmp ebx, 2
    sete dl
    mov rcx, [h_send]
    call EnableWindow
    call update_status
    test ebx, ebx
    jnz .not0
    lea rax, [users_present]        ; getrennt -> Liste leeren
    xor ecx, ecx
    mov [rax], rcx
    mov [rax+8], rcx
    mov [rax+16], rcx
    mov [rax+24], rcx
    mov dword [shown_mask], 0
    call rebuild_users
.not0:
    cmp ebx, 2
    jne .ret
    mov rcx, [h_input]
    call SetFocus
.ret:
    add rsp, 32
    pop rbx
    ret

update_status:
    sub rsp, 40
    lea rdx, [w_st_off]
    cmp byte [g_state], 1
    jne .u1
    lea rdx, [w_st_conn]
.u1:
    cmp byte [g_state], 2
    jne .set
    lea rdx, [w_st_talk]
    cmp byte [talking], 0
    jne .set
    lea rdx, [w_st_muted]
    cmp byte [muted], 0
    jne .set
    lea rdx, [w_st_open]
    cmp byte [open_mic], 0
    jne .set
    lea rdx, [w_st_ptt]
.set:
    mov rcx, [h_status]
    call SetWindowTextW
    add rsp, 40
    ret

; -----------------------------------------------------------------------------
; timer_tick - Statuszeile und "wer spricht"-Anzeige aktualisieren
; -----------------------------------------------------------------------------
timer_tick:
    push rbx
    push rsi
    sub rsp, 40
    movzx eax, byte [talking]
    movzx ecx, byte [muted]
    shl ecx, 1
    or eax, ecx
    movzx ecx, byte [open_mic]
    shl ecx, 2
    or eax, ecx
    movzx ecx, byte [g_state]
    shl ecx, 3
    or eax, ecx
    cmp eax, [shown_key]
    je .speakers
    mov [shown_key], eax
    call update_status
.speakers:
    call GetTickCount64
    mov rsi, rax
    lea r8, [users_present]
    lea r9, [last_spoke]
    xor ecx, ecx                    ; Bitmaske
    xor edx, edx
.l: cmp byte [r8+rdx], 0
    je .n
    mov rax, rsi
    sub rax, [r9+rdx*8]
    cmp rax, SPEAK_HOLD_MS
    jae .n
    bts ecx, edx
.n: inc edx
    cmp edx, MAX_SLOTS
    jb .l
    cmp ecx, [shown_mask]
    je .ret
    mov [shown_mask], ecx
    call rebuild_users
.ret:
    add rsp, 40
    pop rsi
    pop rbx
    ret

; -----------------------------------------------------------------------------
; parse_users - rcx = Liste {Slot, Laenge, Name}..., edx = Laenge
; -----------------------------------------------------------------------------
parse_users:
    push rsi
    push rdi
    lea rax, [users_present]
    xor r8d, r8d
    mov [rax], r8
    mov [rax+8], r8
    mov [rax+16], r8
    mov [rax+24], r8
    mov rsi, rcx
    mov edx, edx
    lea rdi, [rcx+rdx]              ; Ende
.l: lea rax, [rsi+2]
    cmp rax, rdi
    ja .d
    movzx ecx, byte [rsi]
    movzx edx, byte [rsi+1]
    lea rax, [rsi+rdx+2]
    cmp rax, rdi
    ja .d
    cmp ecx, MAX_SLOTS
    jae .skip
    cmp edx, NAME_ENTRY-1
    ja .skip
    imul r8, rcx, NAME_ENTRY
    lea r9, [users_name]
    add r8, r9
    xor r10d, r10d
.c: cmp r10d, edx
    jae .cz
    mov r11b, [rsi+r10+2]
    mov [r8+r10], r11b
    inc r10d
    jmp .c
.cz:
    mov byte [r8+r10], 0
    lea r9, [users_present]
    mov byte [r9+rcx], 1
.skip:
    lea rsi, [rsi+rdx+2]
    jmp .l
.d: pop rdi
    pop rsi
    ret

; rebuild_users - Listbox neu fuellen ("● Name" = spricht gerade)
rebuild_users:
    push rbx
    push rsi
    push rdi
    sub rsp, 48
    SENDMSG h_users, LB_RESETCONTENT, 0, 0
    xor ebx, ebx
.l: lea rax, [users_present]
    cmp byte [rax+rbx], 0
    je .n
    lea rcx, [g_wtmp2]
    lea rdx, [w_idle]
    mov eax, [shown_mask]
    bt eax, ebx
    jnc .p
    lea rdx, [w_talk]
.p: call wcat
    mov rdi, rax
    mov ecx, CP_UTF8
    xor edx, edx
    imul r8, rbx, NAME_ENTRY
    lea rax, [users_name]
    add r8, rax
    mov r9d, -1
    mov [rsp+32], rdi
    mov qword [rsp+40], 48
    call MultiByteToWideChar
    test eax, eax
    jz .n
    mov eax, eax
    lea rdi, [rdi+rax*2-2]
    cmp bl, [my_slot]
    jne .add
    cmp byte [voice_ready], 0
    je .add
    mov rcx, rdi
    lea rdx, [w_you]
    call wcat
.add:
    lea r9, [g_wtmp2]
    SENDMSG h_users, LB_ADDSTRING, 0, r9
.n: inc ebx
    cmp ebx, MAX_SLOTS
    jb .l
    add rsp, 48
    pop rdi
    pop rsi
    pop rbx
    ret

; wcat - rcx = Ziel, rdx = Quelle (UTF-16) -> rax = Zeiger auf die 0
wcat:
    mov rax, rcx
.l: mov r8w, [rdx]
    mov [rax], r8w
    test r8w, r8w
    jz .d
    add rax, 2
    add rdx, 2
    jmp .l
.d: ret

; -----------------------------------------------------------------------------
; append_history - rcx = UTF-8-Zeile, wird mit Uhrzeit angehaengt
; -----------------------------------------------------------------------------
append_history:
    push rbx
    push rsi
    sub rsp, 72
    mov rsi, rcx
    lea rcx, [systime]
    call GetLocalTime
    lea rbx, [g_wtmp]
    mov word [rbx], '['
    movzx eax, word [systime+8]
    xor edx, edx
    mov ecx, 10
    div ecx
    add eax, '0'
    mov [rbx+2], ax
    add edx, '0'
    mov [rbx+4], dx
    mov word [rbx+6], ':'
    movzx eax, word [systime+10]
    xor edx, edx
    div ecx
    add eax, '0'
    mov [rbx+8], ax
    add edx, '0'
    mov [rbx+10], dx
    mov word [rbx+12], ']'
    mov word [rbx+14], ' '
    mov ecx, CP_UTF8
    xor edx, edx
    mov r8, rsi
    mov r9d, -1
    lea rax, [rbx+16]
    mov [rsp+32], rax
    mov qword [rsp+40], 1900
    call MultiByteToWideChar
    test eax, eax
    jnz .conv
    mov eax, 1
.conv:
    mov eax, eax
    lea rdx, [rbx+rax*2+14]         ; Zeiger auf die abschliessende 0
    mov dword [rdx], 0x000A000D     ; CR LF
    mov word [rdx+4], 0

    mov rcx, [h_history]
    call GetWindowTextLengthW
    cmp eax, 200000                 ; Verlauf begrenzen
    jb .append
    SENDMSG h_history, EM_SETSEL, 0, 60000
    lea r9, [w_empty]
    SENDMSG h_history, EM_REPLACESEL, 0, r9
    mov rcx, [h_history]
    call GetWindowTextLengthW
.append:
    mov esi, eax
    SENDMSG h_history, EM_SETSEL, rsi, rsi
    SENDMSG h_history, EM_REPLACESEL, 0, rbx
    SENDMSG h_history, WM_VSCROLL, SB_BOTTOM, 0
    add rsp, 72
    pop rsi
    pop rbx
    ret

; post_text - aus einem Hintergrund-Thread: rcx = UTF-8-Zeile anzeigen
post_text:
    sub rsp, 40
    mov r9, rcx
    mov rcx, [hwnd_main]
    mov edx, WM_APP_TEXT
    xor r8d, r8d
    call SendMessageW
    add rsp, 40
    ret

; post_state - aus einem Hintergrund-Thread: ecx = neuer Zustand
post_state:
    sub rsp, 40
    mov r8d, ecx
    mov rcx, [hwnd_main]
    mov edx, WM_APP_STATE
    xor r9d, r9d
    call SendMessageW
    add rsp, 40
    ret

; -----------------------------------------------------------------------------
; send_chat - Text aus dem Eingabefeld verschluesselt senden
; -----------------------------------------------------------------------------
send_chat:
    sub rsp, 72
    cmp byte [g_state], 2
    jne .ret
    mov rcx, [h_input]
    lea rdx, [g_wtmp]
    mov r8d, 320
    call GetWindowTextW
    test eax, eax
    jz .ret
    mov ecx, CP_UTF8
    xor edx, edx
    lea r8, [g_wtmp]
    mov r9d, -1
    lea rax, [chat_buf+1]
    mov [rsp+32], rax
    mov qword [rsp+40], 1200
    mov qword [rsp+48], 0
    mov qword [rsp+56], 0
    call WideCharToMultiByte
    test eax, eax
    jz .ret
    mov byte [chat_buf], MSG_TEXT
    mov r9d, eax                    ; 1 Typ-Byte + (eax-1) Textbytes
    mov rcx, [g_tcp]
    test rcx, rcx
    jz .ret
    lea rdx, [push_state]
    lea r8, [chat_buf]
    call ss_send
    mov rcx, [h_input]
    lea rdx, [w_empty]
    call SetWindowTextW
.ret:
    add rsp, 72
    ret

; -----------------------------------------------------------------------------
; toggle_connect - Verbinden bzw. Trennen/Abbrechen
; -----------------------------------------------------------------------------
toggle_connect:
    push rbx
    sub rsp, 64
    cmp byte [g_state], 0
    je .connect
    xor eax, eax                    ; Socket atomar uebernehmen und schliessen
    xchg rax, [g_tcp]
    test rax, rax
    jz .ret
    mov rcx, rax
    call closesocket
    jmp .ret
.connect:
    mov rcx, [h_ip]
    lea rdx, [g_wtmp]
    mov r8d, 120
    call GetWindowTextW
    lea rcx, [w_section]
    lea rdx, [w_key_server]
    lea r8, [g_wtmp]
    lea r9, [ini_path]
    call WritePrivateProfileStringW
    mov ecx, CP_UTF8
    xor edx, edx
    lea r8, [g_wtmp]
    mov r9d, -1
    lea rax, [conn_ip]
    mov [rsp+32], rax
    mov qword [rsp+40], 120
    mov qword [rsp+48], 0
    mov qword [rsp+56], 0
    call WideCharToMultiByte
    cmp byte [conn_ip], 0
    jne .ip_ok
    lea rcx, [conn_ip]
    lea rdx, [t_default_ip]
    call strcat_to
.ip_ok:
    mov rcx, [h_name]
    lea rdx, [g_wtmp]
    mov r8d, 40
    call GetWindowTextW
    lea rcx, [w_section]
    lea rdx, [w_key_name]
    lea r8, [g_wtmp]
    lea r9, [ini_path]
    call WritePrivateProfileStringW
    mov byte [name_msg], MSG_NAME
    mov ecx, CP_UTF8
    xor edx, edx
    lea r8, [g_wtmp]
    mov r9d, -1
    lea rax, [name_msg+1]
    mov [rsp+32], rax
    mov qword [rsp+40], 120
    mov qword [rsp+48], 0
    mov qword [rsp+56], 0
    call WideCharToMultiByte
    cmp byte [name_msg+1], 0
    jne .name_ok
    lea rcx, [name_msg+1]
    lea rdx, [t_guest]
    call strcat_to
.name_ok:
    mov ecx, 1
    call set_state
    xor ecx, ecx
    xor edx, edx
    lea r8, [net_thread]
    xor r9d, r9d
    mov qword [rsp+32], 0
    mov qword [rsp+40], 0
    call CreateThread
    mov rcx, rax
    call CloseHandle
.ret:
    add rsp, 64
    pop rbx
    ret

; =============================================================================
; net_thread - Verbindung aufbauen und Nachrichten empfangen
; =============================================================================
net_thread:
    push rbx
    push rsi
    push rdi
    push r12
    sub rsp, 72
    lea rcx, [net_msg]
    lea rdx, [t_connecting]
    call strcat_to
    mov rcx, rax
    lea rdx, [conn_ip]
    call strcat_to
    mov rcx, rax
    lea rdx, [t_dots]
    call strcat_to
    lea rcx, [net_msg]
    call post_text

    lea rcx, [conn_ip]              ; IP oder Hostname
    call inet_addr
    cmp eax, -1
    jne .have_addr
    lea rcx, [conn_ip]
    call gethostbyname
    test rax, rax
    jz .err_resolve
    mov rax, [rax+24]               ; h_addr_list
    mov rax, [rax]
    mov eax, [rax]
.have_addr:
    mov word [srv_addr], AF_INET
    mov word [srv_addr+2], PORT_BE
    mov [srv_addr+4], eax

    mov ecx, AF_INET
    mov edx, SOCK_STREAM
    mov r8d, IPPROTO_TCP
    call socket
    cmp rax, -1
    je .err_connect
    mov rbx, rax
    mov [g_tcp], rax
    mov rcx, rbx
    lea rdx, [srv_addr]
    mov r8d, 16
    call connect
    test eax, eax
    jnz .err_connect

    mov dword [rsp+64], 1           ; TCP_NODELAY: Text sofort senden
    mov rcx, rbx
    mov edx, IPPROTO_TCP
    mov r8d, TCP_NODELAY
    lea r9, [rsp+64]
    mov qword [rsp+32], 4
    call setsockopt
    mov dword [rsp+64], 10000       ; Verbindungsaufbau max. 10 s
    mov rcx, rbx
    mov edx, SOL_SOCKET
    mov r8d, SO_RCVTIMEO
    lea r9, [rsp+64]
    mov qword [rsp+32], 4
    call setsockopt

    mov rcx, rbx
    call client_handshake
    test eax, eax
    jz .err_hs

    mov dword [rsp+64], 0
    mov rcx, rbx
    mov edx, SOL_SOCKET
    mov r8d, SO_RCVTIMEO
    lea r9, [rsp+64]
    mov qword [rsp+32], 4
    call setsockopt

    lea rcx, [hs_buf]               ; Fingerabdruck des Servers anzeigen
    lea rdx, [fp_text]
    call fingerprint
    lea rcx, [net_msg]
    lea rdx, [t_secure]
    call strcat_to
    mov rcx, rax
    lea rdx, [fp_text]
    call strcat_to
    lea rcx, [net_msg]
    call post_text

    lea rcx, [name_msg]             ; Namen senden
    call strlen
    mov r9d, eax
    mov rcx, rbx
    lea rdx, [push_state]
    lea r8, [name_msg]
    call ss_send

    mov ecx, AF_INET                ; UDP fuer Sprache
    mov edx, SOCK_DGRAM
    mov r8d, IPPROTO_UDP
    call socket
    mov [g_udp], rax
    mov rcx, rax
    lea rdx, [srv_addr]
    mov r8d, 16
    call connect
    mov rcx, [g_udp]
    call udp_no_connreset
    mov qword [udp_tx_ctr], 0
    mov qword [udp_rx_ctr], 0

    mov ecx, 2
    call post_state

.loop:
    mov rcx, rbx
    lea rdx, [pull_state]
    lea r8, [net_rbuf]
    call ss_recv
    test rax, rax
    js .lost
    jz .loop
    mov r12, rax
    mov al, [net_rbuf]
    cmp al, MSG_TEXT
    je .m_text
    cmp al, MSG_USERS
    je .m_users
    cmp al, MSG_WELCOME
    jne .loop
    mov al, [net_rbuf+1]            ; eigener Slot -> Sprache freischalten
    mov [my_slot], al
    mov qword [last_send], 0
    mov byte [voice_ready], 1
    jmp .loop
.m_text:
    lea rcx, [net_rbuf+1]
    call post_text
    jmp .loop
.m_users:
    mov rcx, [hwnd_main]
    mov edx, WM_APP_USERS
    lea r8, [r12-1]
    lea r9, [net_rbuf+1]
    call SendMessageW
    jmp .loop

.err_resolve:
    lea rcx, [t_err_resolve]
    jmp .fail
.err_connect:
    lea rcx, [t_err_connect]
    jmp .fail
.err_hs:
    lea rcx, [t_err_hs]
.fail:
    call post_text
    jmp .cleanup
.lost:
    lea rcx, [t_lost]
    call post_text
.cleanup:
    mov byte [voice_ready], 0
    xor eax, eax
    xchg rax, [g_udp]
    test rax, rax
    jz .c1
    mov rcx, rax
    call closesocket
.c1:
    xor eax, eax
    xchg rax, [g_tcp]
    test rax, rax
    jz .c2
    mov rcx, rax
    call closesocket
.c2:
    lea rcx, [c_sk]                 ; Schluesselmaterial loeschen
    mov edx, net_rbuf - c_sk
    call sodium_memzero
    xor ecx, ecx
    call post_state
    xor eax, eax
    add rsp, 72
    pop r12
    pop rdi
    pop rsi
    pop rbx
    ret

; -----------------------------------------------------------------------------
; client_handshake - rcx = Socket -> eax 1 = sicher verbunden
; -----------------------------------------------------------------------------
client_handshake:
    push rbx
    sub rsp, 48
    mov rbx, rcx
    lea rcx, [c_pk]
    lea rdx, [c_sk]
    call crypto_kx_keypair
    mov rcx, rbx
    lea rdx, [c_pk]
    mov r8d, KEY_BYTES
    call send_all
    test eax, eax
    jz .fail
    mov rcx, rbx
    lea rdx, [hs_buf]
    mov r8d, HS_REPLY
    call recv_all
    test eax, eax
    jz .fail
    ; Signatur pruefen: Sig(ClientPK | ServerEph) mit dem Server-Schluessel
    lea rcx, [hs_msg]
    lea rdx, [c_pk]
    mov r8d, 32
    call memcpy
    lea rcx, [hs_msg+32]
    lea rdx, [hs_buf+32]
    mov r8d, 32
    call memcpy
    lea rcx, [hs_buf+64]
    lea rdx, [hs_msg]
    mov r8d, 64
    lea r9, [hs_buf]
    call crypto_sign_verify_detached
    test eax, eax
    jnz .fail
    lea rcx, [k_rx]
    lea rdx, [k_tx]
    lea r8, [c_pk]
    lea r9, [c_sk]
    lea rax, [hs_buf+32]
    mov [rsp+32], rax
    call crypto_kx_client_session_keys
    test eax, eax
    jnz .fail
    lea rcx, [udp_rx_key]
    lea rdx, [k_rx]
    call derive_udp_key
    lea rcx, [udp_tx_key]
    lea rdx, [k_tx]
    call derive_udp_key
    mov rcx, rbx
    lea rdx, [ss_hdr]
    mov r8d, SS_HEADER
    call recv_all
    test eax, eax
    jz .fail
    lea rcx, [pull_state]
    lea rdx, [ss_hdr]
    lea r8, [k_rx]
    call crypto_secretstream_xchacha20poly1305_init_pull
    test eax, eax
    jnz .fail
    lea rcx, [push_state]
    lea rdx, [ss_hdr]
    lea r8, [k_tx]
    call crypto_secretstream_xchacha20poly1305_init_push
    mov rcx, rbx
    lea rdx, [ss_hdr]
    mov r8d, SS_HEADER
    call send_all
    test eax, eax
    jz .fail
    lea rcx, [c_sk]
    mov edx, 32
    call sodium_memzero
    lea rcx, [k_rx]
    mov edx, 64
    call sodium_memzero
    mov eax, 1
    jmp .ret
.fail:
    xor eax, eax
.ret:
    add rsp, 48
    pop rbx
    ret

; memcpy - rcx = Ziel, rdx = Quelle, r8 = Laenge
memcpy:
    test r8, r8
    jz .d
.l: mov al, [rdx]
    mov [rcx], al
    inc rcx
    inc rdx
    dec r8
    jnz .l
.d: ret

; =============================================================================
; Audio
; =============================================================================
audio_init:
    push rbx
    push rsi
    sub rsp, 56
    lea rcx, [crit]
    call InitializeCriticalSection
    call make_event
    mov [ev_in], rax
    call make_event
    mov [ev_out], rax

    ; Opus-Encoder: 48 kHz mono, VOIP, 32 kbit/s, Signal = Sprache
    mov ecx, 48000
    mov edx, 1
    mov r8d, 2048                   ; OPUS_APPLICATION_VOIP
    lea r9, [opus_err]
    call opus_encoder_create
    mov [encoder], rax
    test rax, rax
    jz .dec
    mov rcx, rax
    mov edx, 4002                   ; OPUS_SET_BITRATE
    mov r8d, 32000
    call opus_encoder_ctl
    mov rcx, [encoder]
    mov edx, 4024                   ; OPUS_SET_SIGNAL
    mov r8d, 3001                   ; OPUS_SIGNAL_VOICE
    call opus_encoder_ctl
.dec:
    ; ein Decoder pro moeglichem Sprecher
    lea rbx, [decoders]
    mov esi, MAX_SLOTS
.dl:
    mov ecx, 48000
    mov edx, 1
    lea r8, [opus_err]
    call opus_decoder_create
    mov [rbx], rax
    add rbx, 8
    dec esi
    jnz .dl

    lea rcx, [hwo]
    mov edx, WAVE_MAPPER
    lea r8, [wave_fmt]
    mov r9, [ev_out]
    mov qword [rsp+32], 0
    mov qword [rsp+40], CALLBACK_EVENT
    call waveOutOpen
    test eax, eax
    jz .out_ok
    mov qword [hwo], 0
    jmp .in
.out_ok:
    mov byte [audio_out_ok], 1
    lea rcx, [play_thread]
    call make_thread
.in:
    lea rcx, [hwi]
    mov edx, WAVE_MAPPER
    lea r8, [wave_fmt]
    mov r9, [ev_in]
    mov qword [rsp+32], 0
    mov qword [rsp+40], CALLBACK_EVENT
    call waveInOpen
    test eax, eax
    jz .in_ok
    mov qword [hwi], 0
    jmp .threads
.in_ok:
    cmp qword [encoder], 0
    je .threads
    mov byte [audio_in_ok], 1
.threads:
    lea rcx, [capture_thread]
    call make_thread
    lea rcx, [udp_thread]
    call make_thread
    add rsp, 56
    pop rsi
    pop rbx
    ret

make_thread:
    sub rsp, 56
    mov r8, rcx
    xor ecx, ecx
    xor edx, edx
    xor r9d, r9d
    mov qword [rsp+32], 0
    mov qword [rsp+40], 0
    call CreateThread
    mov rcx, rax
    call CloseHandle
    add rsp, 56
    ret

make_event:
    sub rsp, 40
    xor ecx, ecx
    xor edx, edx
    xor r8d, r8d
    xor r9d, r9d
    call CreateEventA
    add rsp, 40
    ret

; -----------------------------------------------------------------------------
; udp_thread - Sprache empfangen -> pruefen -> Opus dekodieren -> mischen
; -----------------------------------------------------------------------------
udp_thread:
    sub rsp, 88
.l: cmp byte [voice_ready], 0
    jne .r
    mov ecx, 50
    call Sleep
    jmp .l
.r: mov rcx, [g_udp]
    lea rdx, [u_in]
    mov r8d, 2048
    xor r9d, r9d
    call recv
    cmp eax, 0
    jg .got
    mov ecx, 20
    call Sleep
    jmp .l
.got:
    mov r12d, eax
    cmp byte [u_in], 1
    jne .l
    movzx ebx, byte [u_in+1]
    cmp ebx, MAX_SLOTS
    jae .l
    mov rax, [u_in+2]               ; Replay-Schutz
    cmp rax, [udp_rx_ctr]
    jbe .l
    lea rcx, [u_in]
    mov edx, r12d
    lea r8, [u_plain]
    lea r9, [udp_rx_key]
    call udp_open
    test rax, rax
    js .l
    jz .l
    mov r13, rax
    mov rax, [u_in+2]
    mov [udp_rx_ctr], rax

    lea rax, [decoders]
    mov rcx, [rax+rbx*8]
    test rcx, rcx
    jz .l
    lea rdx, [u_plain]
    mov r8d, r13d
    lea r9, [pcm_dec]
    mov qword [rsp+32], FRAME_SAMPLES
    mov qword [rsp+40], 0
    call opus_decode
    cmp eax, FRAME_SAMPLES
    jne .l
    call GetTickCount64
    lea rcx, [last_spoke]
    mov [rcx+rbx*8], rax
    cmp qword [hwo], 0
    je .l

    lea rcx, [crit]
    call EnterCriticalSection
    lea rsi, [cursors]
    mov rax, [rsi+rbx*8]
    mov rdx, [playpos]
    cmp rax, rdx                    ; zu spaet -> neu einsetzen
    jb .reset
    lea r8, [rdx + RING_SAMPLES/2]  ; zu weit voraus -> neu einsetzen
    cmp rax, r8
    jbe .pos_ok
.reset:
    lea rax, [rdx + JITTER_SAMPLES]
.pos_ok:
    lea rcx, [rax + FRAME_SAMPLES]
    mov [rsi+rbx*8], rcx
    lea rdi, [ring]
    lea r9, [pcm_dec]
    mov r11, rax
    and r11d, RING_MASK
    cmp r11d, RING_SAMPLES - FRAME_SAMPLES
    ja .scalar
    ; schneller Weg: 8 Samples auf einmal mit Saettigung addieren (SSE2)
    lea rdi, [rdi+r11*2]
    mov ecx, FRAME_SAMPLES / 8
.sse:
    movdqu xmm0, [rdi]
    movdqu xmm1, [r9]
    paddsw xmm0, xmm1
    movdqu [rdi], xmm0
    add rdi, 16
    add r9, 16
    dec ecx
    jnz .sse
    jmp .mixed
.scalar:                            ; Ringpuffer-Umbruch: einzeln
    mov ecx, FRAME_SAMPLES
.sc:
    mov r11, rax
    and r11d, RING_MASK
    movsx edx, word [rdi+r11*2]
    movsx r8d, word [r9]
    add edx, r8d
    cmp edx, 32767
    jle .c1
    mov edx, 32767
.c1:
    cmp edx, -32768
    jge .c2
    mov edx, -32768
.c2:
    mov [rdi+r11*2], dx
    add r9, 2
    inc rax
    dec ecx
    jnz .sc
.mixed:
    lea rcx, [crit]
    call LeaveCriticalSection
    jmp .l

; -----------------------------------------------------------------------------
; play_thread - Ringpuffer -> Lautsprecher
; -----------------------------------------------------------------------------
play_thread:
    sub rsp, 88
    lea rbx, [out_hdrs]
    lea rsi, [out_bufs]
    mov r12d, NBUF
.init:
    mov [rbx], rsi
    mov dword [rbx+8], FRAME_BYTES
    mov dword [rbx+24], 0
    mov rcx, [hwo]
    mov rdx, rbx
    mov r8d, WAVEHDR_SIZE
    call waveOutPrepareHeader
    mov rcx, [hwo]
    mov rdx, rbx
    mov r8d, WAVEHDR_SIZE
    call waveOutWrite
    add rbx, WAVEHDR_SIZE
    add rsi, FRAME_BYTES
    dec r12d
    jnz .init
.wait:
    mov rcx, [ev_out]
    mov edx, -1
    call WaitForSingleObject
    lea rbx, [out_hdrs]
    mov r12d, NBUF
.chk:
    test dword [rbx+24], WHDR_DONE
    jz .next
    mov rcx, [rbx]
    call mix_out
    and dword [rbx+24], ~WHDR_DONE
    mov rcx, [hwo]
    mov rdx, rbx
    mov r8d, WAVEHDR_SIZE
    call waveOutWrite
.next:
    add rbx, WAVEHDR_SIZE
    dec r12d
    jnz .chk
    jmp .wait

; mix_out - rcx = Zielpuffer: naechste 20 ms aus dem Ring holen und loeschen
mix_out:
    push rbx
    push rsi
    push rdi
    sub rsp, 32
    mov rbx, rcx
    lea rcx, [crit]
    call EnterCriticalSection
    lea rsi, [ring]
    mov rdx, [playpos]
    mov rdi, rbx
    mov eax, edx
    and eax, RING_MASK
    cmp eax, RING_SAMPLES - FRAME_SAMPLES
    ja .scalar
    lea rsi, [rsi+rax*2]            ; zusammenhaengend: 16 Byte pro Schritt
    pxor xmm1, xmm1
    mov ecx, FRAME_SAMPLES / 8
.sse:
    movdqu xmm0, [rsi]
    movdqu [rdi], xmm0
    movdqu [rsi], xmm1
    add rsi, 16
    add rdi, 16
    dec ecx
    jnz .sse
    add rdx, FRAME_SAMPLES
    jmp .done
.scalar:
    mov ecx, FRAME_SAMPLES
.l: mov rax, rdx
    and eax, RING_MASK
    mov r8w, [rsi+rax*2]
    mov [rdi], r8w
    mov word [rsi+rax*2], 0
    add rdi, 2
    inc rdx
    dec ecx
    jnz .l
.done:
    mov [playpos], rdx
    lea rcx, [crit]
    call LeaveCriticalSection
    add rsp, 32
    pop rdi
    pop rsi
    pop rbx
    ret

; -----------------------------------------------------------------------------
; capture_thread - Mikrofon -> Opus -> verschluesselt an den Server
; -----------------------------------------------------------------------------
capture_thread:
    sub rsp, 88
    cmp byte [audio_in_ok], 0
    je .nomic
    lea rbx, [in_hdrs]
    lea rsi, [in_bufs]
    mov r12d, NBUF
.init:
    mov [rbx], rsi
    mov dword [rbx+8], FRAME_BYTES
    mov dword [rbx+24], 0
    mov rcx, [hwi]
    mov rdx, rbx
    mov r8d, WAVEHDR_SIZE
    call waveInPrepareHeader
    mov rcx, [hwi]
    mov rdx, rbx
    mov r8d, WAVEHDR_SIZE
    call waveInAddBuffer
    add rbx, WAVEHDR_SIZE
    add rsi, FRAME_BYTES
    dec r12d
    jnz .init
    mov rcx, [hwi]
    call waveInStart

.wait:
    mov rcx, [ev_in]
    mov edx, 200
    call WaitForSingleObject
    lea rbx, [in_hdrs]
    mov r12d, NBUF
.chk:
    test dword [rbx+24], WHDR_DONE
    jz .next
    call want_talk
    test eax, eax
    jz .requeue
    cmp dword [rbx+12], FRAME_BYTES ; dwBytesRecorded
    jne .requeue
    mov rcx, [encoder]
    mov rdx, [rbx]
    mov r8d, FRAME_SAMPLES
    lea r9, [opus_out]
    mov qword [rsp+32], OPUS_MAX
    call opus_encode
    cmp eax, 0
    jle .requeue
    mov r13d, eax
    mov byte [c_out], 1
    mov al, [my_slot]
    mov [c_out+1], al
    inc qword [udp_tx_ctr]
    lea rcx, [c_out]
    mov rdx, [udp_tx_ctr]
    lea r8, [opus_out]
    mov r9d, r13d
    lea rax, [udp_tx_key]
    mov [rsp+32], rax
    call udp_seal
    mov rcx, [g_udp]
    lea rdx, [c_out]
    mov r8d, eax
    xor r9d, r9d
    call send
    call GetTickCount64
    mov [last_send], rax
    movzx ecx, byte [my_slot]
    lea rdx, [last_spoke]
    mov [rdx+rcx*8], rax
.requeue:
    and dword [rbx+24], ~WHDR_DONE
    mov rcx, [hwi]
    mov rdx, rbx
    mov r8d, WAVEHDR_SIZE
    call waveInAddBuffer
.next:
    add rbx, WAVEHDR_SIZE
    dec r12d
    jnz .chk
    call keepalive
    jmp .wait

.nomic:
    call keepalive
    mov ecx, 200
    call Sleep
    jmp .nomic

; keepalive - verschluesseltes Lebenszeichen, damit der Server unsere
; UDP-Adresse kennt (auch wenn wir nur zuhoeren)
keepalive:
    sub rsp, 56
    cmp byte [voice_ready], 0
    je .done
    call GetTickCount64
    mov rcx, rax
    sub rcx, [last_send]
    cmp rcx, KEEPALIVE_MS
    jb .done
    mov [last_send], rax
    mov byte [c_out], 0
    mov al, [my_slot]
    mov [c_out+1], al
    inc qword [udp_tx_ctr]
    lea rcx, [c_out]
    mov rdx, [udp_tx_ctr]
    lea r8, [opus_out]
    xor r9d, r9d
    lea rax, [udp_tx_key]
    mov [rsp+32], rax
    call udp_seal
    mov rcx, [g_udp]
    lea rdx, [c_out]
    mov r8d, eax
    xor r9d, r9d
    call send
.done:
    add rsp, 56
    ret

; want_talk - eax = 1 wenn gesendet werden soll (setzt auch 'talking')
want_talk:
    sub rsp, 40
    xor eax, eax
    cmp byte [voice_ready], 0
    je .set
    cmp byte [muted], 0
    jne .set
    mov eax, 1
    cmp byte [open_mic], 0
    jne .set
    mov ecx, VK_F2
    call GetAsyncKeyState
    test ax, 0x8000
    setnz al
    movzx eax, al
.set:
    mov [talking], al
    add rsp, 40
    ret
