; boot-craft's FAT32 boot code, for Windows install drives booted by a BIOS.
;
; Two pieces, as in Windows' own FAT32 boot record: the boot sector (bytes
; 90-509 of sector 0, around the BPB boot-craft writes) and reserved
; sector 2 (bytes 0-507), which the first loads to 0:8000. Together they find
; BOOTMGR in the root folder, follow its cluster chain loading it to
; 2000:0000, and start it the way GRUB's ntldr command does: DL = the drive,
; DS = ES = SS = 0, SP = SI = 7C00, the boot sector (with its BPB) at 0:7C00.
;
; Any sector size works: everything is in units of the BPB's.
;
; Build: nasm -f bin -o fat32.bin fat32.asm   (1024 bytes: both pieces)
bits 16
cpu 386
org 0x7c00

%define bps      0x7c0b         ; word: bytes per sector
%define spc      0x7c0d         ; byte: sectors per cluster
%define reserved 0x7c0e         ; word
%define fats     0x7c10         ; byte
%define hidden   0x7c1c         ; dword: partition start
%define fat_size 0x7c24         ; dword: sectors per FAT
%define root     0x7c2c         ; dword: root folder cluster
%define drive    0x7c40         ; byte

FAT_BUF  equ 0x1000             ; a FAT sector (0x1000 bytes at most)
DIR_BUF  equ 0x3000             ; a root folder sector
LOAD_SEG equ 0x2000             ; BOOTMGR goes here

    jmp short start
    nop
    times 90 - ($ - $$) db 0    ; the BPB (boot-craft writes it)

start:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7c00
    sti
    cld
    mov [drive], dl
    mov ah, 0x41
    mov bx, 0x55aa
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
    mov eax, 2
    mov bx, 0x0800
    call read
    cmp dword [0x8000 + 508], 0xaa550000
    jne .bad
    jmp stage2
.bad:
    mov si, msg_bad
    jmp fail

; Reads sector EAX of the partition to BX:0000. Keeps every register.
read:
    pushad
    add eax, [hidden]
    mov [dap.lba], eax
    mov [dap.seg], bx
    mov di, 3
.try:
    mov si, dap
    mov ah, 0x42
    mov dl, [drive]
    int 0x13
    jnc .done
    xor ah, ah
    int 0x13
    dec di
    jnz .try
    mov si, msg_read
    jmp fail
.done:
    popad
    ret

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
    dw 1
    dw 0
.seg:
    dw 0
.lba:
    dd 0, 0

msg_lba:  db "This BIOS can't read the drive (no LBA disk reads).", 13, 10, 0
msg_read: db "Disk read error.", 13, 10, 0
msg_bad:  db "The boot code is damaged: make the drive again.", 13, 10, 0

    times 510 - ($ - $$) db 0
    dw 0xaa55

section stage2 follows=.text vstart=0x8000

stage2:
    ; Where the clusters start, relative to the partition.
    movzx eax, byte [fats]
    mul dword [fat_size]
    movzx ebx, word [reserved]
    add eax, ebx
    mov [data_start], eax
    mov ax, [bps]
    shr ax, 4
    mov [sector_paras], ax
    int 0x12                    ; KiB of low memory
    shl ax, 6
    mov [mem_seg], ax

    mov eax, [root]
.cluster:
    cmp eax, 2
    jb missing
    mov [cluster], eax
    call first_sector
.sector:
    mov bx, DIR_BUF >> 4
    call read
    mov di, DIR_BUF
.entry:
    cmp byte [di], 0
    je missing
    test byte [di + 11], 0x18   ; a folder, label or long name
    jnz .next
    push cx
    push di
    mov si, name
    mov cx, 11
    repe cmpsb
    pop di
    pop cx
    je found
.next:
    add di, 32
    mov dx, di
    sub dx, DIR_BUF
    cmp dx, [bps]
    jb .entry
    inc eax
    loop .sector
    mov eax, [cluster]
    call next_cluster
    cmp eax, 0x0ffffff8
    jb .cluster
missing:
    mov si, msg_missing
    jmp fail

found:
    ; Sectors to load: the size rounded up.
    mov eax, [di + 28]
    movzx ebx, word [bps]
    add eax, ebx
    dec eax
    xor edx, edx
    div ebx
    mov [left], eax
    or eax, eax
    jz missing
    mov ax, [di + 20]
    shl eax, 16
    mov ax, [di + 26]
    mov bx, LOAD_SEG
.cluster:
    cmp eax, 2
    jb missing
    cmp eax, 0x0ffffff8
    jae missing                 ; the chain ended before the file did
    mov [cluster], eax
    call first_sector
.sector:
    mov dx, bx
    add dx, [sector_paras]
    jc too_big
    cmp dx, [mem_seg]
    ja too_big
    call read
    mov bx, dx
    dec dword [left]
    jz start_it
    inc eax
    loop .sector
    mov eax, [cluster]
    call next_cluster
    jmp .cluster

start_it:
    mov dl, [drive]
    xor ax, ax
    mov ds, ax
    mov es, ax
    cli
    mov ss, ax
    mov sp, 0x7c00
    sti
    mov si, 0x7c00
    jmp LOAD_SEG:0

too_big:
    mov si, msg_big
    jmp fail

; EAX = a cluster -> EAX = its first sector (in the partition), CX = sectors
; per cluster.
first_sector:
    sub eax, 2
    movzx ecx, byte [spc]
    mul ecx
    add eax, [data_start]
    ret

; EAX = a cluster -> EAX = the next one in its chain.
next_cluster:
    push bx
    push cx
    shl eax, 2
    movzx ebx, word [bps]
    xor edx, edx
    div ebx
    movzx ebx, word [reserved]
    add eax, ebx
    cmp eax, [fat_sector]
    je .have
    mov [fat_sector], eax
    mov bx, FAT_BUF >> 4
    call read
.have:
    mov bx, dx
    mov eax, [FAT_BUF + bx]
    and eax, 0x0fffffff
    pop cx
    pop bx
    ret

name:         db "BOOTMGR    "
data_start:   dd 0
cluster:      dd 0
left:         dd 0
fat_sector:   dd 0xffffffff
sector_paras: dw 0
mem_seg:      dw 0

msg_missing: db "No BOOTMGR on this drive: start it in UEFI mode.", 13, 10, 0
msg_big:     db "BOOTMGR is too big to load.", 13, 10, 0

    times 508 - ($ - $$) db 0
    dd 0xaa550000
