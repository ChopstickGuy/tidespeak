; =============================================================================
; common.inc - gemeinsame Konstanten und Hilfsroutinen fuer Server und Client
; Windows x64, NASM-Syntax. Aufrufkonvention: Microsoft x64
;   Parameter: rcx, rdx, r8, r9, danach Stack ab [rsp+32]
;   32 Byte "Shadow Space" vor jedem Aufruf, rsp muss 16-Byte-ausgerichtet sein
;   Nicht-fluechtige Register: rbx, rbp, rsi, rdi, r12-r15
; =============================================================================

bits 64
default rel

%define AF_INET            2
%define SOCK_STREAM        1
%define SOCK_DGRAM         2
%define IPPROTO_TCP        6
%define IPPROTO_UDP        17
%define STD_INPUT_HANDLE   -10
%define STD_OUTPUT_HANDLE  -11

%define PORT               9987
%define PORT_BE            (((PORT >> 8) & 0xFF) | ((PORT & 0xFF) << 8))   ; htons

%define NICK_MAX           24

; UDP-Pakettypen (1. Byte jedes Pakets)
%define PKT_KEEPALIVE      0
%define PKT_AUDIO          1        ; [1][Absender-Slot][640 Byte PCM]

extern ExitProcess, GetStdHandle, WriteFile, ReadFile
extern WSAIoctl

section .bss
hStdOut     resq 1
hStdIn      resq 1
die_buf     resb 64

section .data
txt_press_enter db "Enter druecken zum Beenden...",13,10,0

section .text

; -----------------------------------------------------------------------------
; init_console - holt die Handles fuer Ein- und Ausgabe
; -----------------------------------------------------------------------------
init_console:
    sub rsp, 40
    mov ecx, STD_OUTPUT_HANDLE
    call GetStdHandle
    mov [hStdOut], rax
    mov ecx, STD_INPUT_HANDLE
    call GetStdHandle
    mov [hStdIn], rax
    add rsp, 40
    ret

; -----------------------------------------------------------------------------
; strlen - rcx = Zeichenkette -> rax = Laenge
; -----------------------------------------------------------------------------
strlen:
    xor eax, eax
.l: cmp byte [rcx+rax], 0
    je .d
    inc rax
    jmp .l
.d: ret

; -----------------------------------------------------------------------------
; print_n - rcx = Puffer, rdx = Laenge -> Ausgabe auf die Konsole
; -----------------------------------------------------------------------------
print_n:
    sub rsp, 56
    mov r8, rdx
    mov rdx, rcx
    mov rcx, [hStdOut]
    lea r9, [rsp+40]
    mov qword [rsp+32], 0
    call WriteFile
    add rsp, 56
    ret

; -----------------------------------------------------------------------------
; print - rcx = nullterminierte Zeichenkette
; -----------------------------------------------------------------------------
print:
    push rbx
    sub rsp, 32
    mov rbx, rcx
    call strlen
    mov rcx, rbx
    mov rdx, rax
    call print_n
    add rsp, 32
    pop rbx
    ret

; -----------------------------------------------------------------------------
; strcat_to - kopiert rdx nach rcx, rax = Zeiger auf die abschliessende 0
; (so lassen sich mehrere Teile hintereinander anhaengen)
; -----------------------------------------------------------------------------
strcat_to:
    mov rax, rcx
.l: mov r8b, [rdx]
    mov [rax], r8b
    test r8b, r8b
    jz .d
    inc rax
    inc rdx
    jmp .l
.d: ret

; -----------------------------------------------------------------------------
; strip_line - schneidet bei erstem Steuerzeichen (CR/LF) ab, rax = Laenge
; -----------------------------------------------------------------------------
strip_line:
    xor eax, eax
.l: cmp byte [rcx+rax], 32
    jb .d
    inc rax
    jmp .l
.d: mov byte [rcx+rax], 0
    ret

; -----------------------------------------------------------------------------
; streq - vergleicht rcx und rdx, eax = 1 wenn gleich
; -----------------------------------------------------------------------------
streq:
.l: mov al, [rcx]
    cmp al, [rdx]
    jne .no
    test al, al
    jz .yes
    inc rcx
    inc rdx
    jmp .l
.yes:
    mov eax, 1
    ret
.no:
    xor eax, eax
    ret

; -----------------------------------------------------------------------------
; udp_no_connreset - rcx = UDP-Socket
; Windows meldet sonst nach einem ICMP "Port unreachable" WSAECONNRESET
; beim naechsten recvfrom - das wuerde den Server stoeren.
; -----------------------------------------------------------------------------
udp_no_connreset:
    sub rsp, 88
    mov dword [rsp+80], 0           ; FALSE
    mov edx, 0x9800000C             ; SIO_UDP_CONNRESET
    lea r8, [rsp+80]
    mov r9d, 4
    mov qword [rsp+32], 0
    mov qword [rsp+40], 0
    lea rax, [rsp+72]
    mov [rsp+48], rax
    mov qword [rsp+56], 0
    mov qword [rsp+64], 0
    call WSAIoctl
    add rsp, 88
    ret

; -----------------------------------------------------------------------------
; die - rcx = Fehlermeldung. Zeigt sie an, wartet auf Enter, beendet.
; -----------------------------------------------------------------------------
die:
    sub rsp, 56
    call print
    lea rcx, [txt_press_enter]
    call print
    mov rcx, [hStdIn]
    lea rdx, [die_buf]
    mov r8d, 64
    lea r9, [rsp+48]
    mov qword [rsp+32], 0
    call ReadFile
    mov ecx, 1
    call ExitProcess
