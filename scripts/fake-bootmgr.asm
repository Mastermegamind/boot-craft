; A stand-in for Windows' BOOTMGR, for scripts/windows-usb-test.sh: checks it
; was started the way BOOTMGR expects (CS = 2000, DL = a hard drive, the
; FAT32 boot sector at 0:7C00, SS:SP = 0:7C00) and that all of it was loaded,
; then prints a marker on the screen (QEMU's SeaBIOS copies it to the
; serial console).
;
; The test appends SECTORS - 1 sectors after this one, sector n starting
; with the dword n and ending with its complement, so a sector loaded
; twice, out of order or not at all is caught.
;
;   nasm -f bin -DSECTORS=900 -o bootmgr fake-bootmgr.asm
bits 16
cpu 386
org 0

    mov ax, cs
    cmp ax, 0x2000
    jne bad
    cmp dl, 0x80
    jb bad
    mov ax, ss
    or ax, ax
    jnz bad
    cmp sp, 0x7c00
    jne bad
    cmp si, 0x7c00
    jne bad
    xor ax, ax
    mov ds, ax
    cmp dword [0x7c00 + 82], "FAT3"
    jne bad
    cmp word [0x7c00 + 510], 0xaa55
    jne bad

    mov ecx, 1
    mov bx, 0x2000 + 32
.sector:
    mov ds, bx
    cmp [0], ecx
    jne bad
    mov eax, ecx
    not eax
    cmp [508], eax
    jne bad
    add bx, 32
    inc ecx
    cmp ecx, SECTORS
    jb .sector

    mov si, ok
    jmp say
bad:
    mov si, no
say:
    mov ax, cs
    mov ds, ax
.next:
    lodsb
    or al, al
    jz .stop
    mov ah, 0x0e
    mov bx, 7
    int 0x10
    jmp .next
.stop:
    hlt
    jmp .stop

ok: db "BOOT-CRAFT-BIOS-OK", 13, 10, 0
no: db "BOOT-CRAFT-BIOS-BAD", 13, 10, 0

    times 512 - ($ - $$) db 0
