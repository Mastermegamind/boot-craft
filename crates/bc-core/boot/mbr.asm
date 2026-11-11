; boot-craft's MBR boot code (the first 440 bytes of the drive).
;
; Like DOS's and Windows': moves itself out of the way, finds the active
; partition, loads its first sector to 0:7C00 and starts it with DL = the
; drive and DS:SI = its partition entry. On a GPT drive's protective MBR it
; says the drive starts in UEFI mode only, rather than running nothing.
;
; Build: nasm -f bin -o mbr.bin mbr.asm   (build.sh does both files)
bits 16
cpu 386
org 0x0600

start:
    cli
    xor ax, ax
    mov ss, ax
    mov sp, 0x7c00
    mov ds, ax
    mov es, ax
    sti
    cld
    mov si, 0x7c00
    mov di, 0x0600
    mov cx, 256
    rep movsw
    jmp 0:moved

moved:
    mov [drive], dl
    mov si, table
    mov cx, 4
.find:
    test byte [si], 0x80
    jnz found
    cmp byte [si + 4], 0xee
    je .gpt
    add si, 16
    loop .find
    mov si, msg_none
    jmp fail
.gpt:
    mov si, msg_gpt
    jmp fail

found:
    mov [part], si
    mov ah, 0x41
    mov bx, 0x55aa
    mov dl, [drive]
    int 0x13
    jc .no_lba
    cmp bx, 0xaa55
    jne .no_lba
    test cl, 1
    jnz .lba
.no_lba:
    mov si, msg_lba
    jmp fail
.lba:
    mov si, [part]
    mov eax, [si + 8]
    mov [dap.lba], eax
    mov di, 3
.try:
    mov si, dap
    mov ah, 0x42
    mov dl, [drive]
    int 0x13
    jnc .read
    xor ah, ah
    int 0x13
    dec di
    jnz .try
    mov si, msg_read
    jmp fail
.read:
    cmp word [0x7dfe], 0xaa55
    je .go
    mov si, msg_empty
    jmp fail
.go:
    mov si, [part]
    mov dl, [drive]
    jmp 0:0x7c00

; Prints the text at SI and stops.
fail:
    lodsb
    or al, al
    jz .stop
    mov ah, 0x0e
    mov bx, 7
    int 0x10
    jmp fail
.stop:
    hlt
    jmp .stop

dap:
    db 16, 0
    dw 1                ; sectors
    dw 0x7c00, 0        ; offset, segment
.lba:
    dd 0, 0

drive: db 0
part: dw 0

msg_gpt:   db "This drive starts in UEFI mode only. For a BIOS PC, make it with --scheme mbr.", 13, 10, 0
msg_none:  db "No active partition on this drive.", 13, 10, 0
msg_lba:   db "This BIOS can't read the drive (no LBA disk reads).", 13, 10, 0
msg_read:  db "Disk read error.", 13, 10, 0
msg_empty: db "The active partition has no boot code.", 13, 10, 0

    times 440 - ($ - $$) db 0
table equ 0x0600 + 446
