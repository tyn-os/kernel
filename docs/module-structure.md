# Module Structure

```mermaid
graph TD
    subgraph Boot
        MS[multiboot.S<br/>113 lines]
        BR[boot.rs<br/>38 lines]
        MN[main.rs<br/>263 lines]
        LB[lib.rs<br/>30 lines]
    end

    subgraph "SMP & Scheduling"
        SC[sched.rs<br/>747 lines]
        IR[interrupts.rs<br/>205 lines]
        SM[smp.rs<br/>179 lines]
        PC[percpu.rs<br/>116 lines]
        AC[acpi.rs<br/>179 lines]
        AP[apic.rs<br/>254 lines]
    end

    subgraph "Syscall Interface"
        SY[syscall.rs<br/>1,242 lines]
    end

    subgraph "I/O & Storage"
        SR[serial.rs<br/>111 lines]
        PP[pipe.rs<br/>240 lines]
        VF[vfs.rs<br/>445 lines]
        EL[elf.rs<br/>175 lines]
    end

    subgraph Memory
        HP[heap.rs<br/>26 lines]
        TH[thread.rs<br/>404 lines]
    end

    subgraph "Networking"
        NM[net/mod.rs<br/>100 lines]
        NS[net/socket.rs<br/>565 lines]
        ND[net/device.rs<br/>135 lines]
        NI[net/interface.rs<br/>38 lines]
        NT[net/tcp_echo.rs<br/>73 lines]
        VH[virtio/hal.rs<br/>52 lines]
    end

    MS --> BR --> MN
    MN --> SC
    MN --> SY
    MN --> VF
    MN --> NM
    MN --> SM
    MN --> AC

    SY --> SC
    SY --> VF
    SY --> PP
    SY --> NS

    IR --> SC
    SM --> SC
    SM --> PC
    SM --> AP
    AC --> AP

    NM --> NS
    NM --> ND
    ND --> VH
    NM --> NI

    style SY fill:#fff3e0,stroke:#e65100
    style SC fill:#e3f2fd,stroke:#1565c0
    style NS fill:#e8f5e9,stroke:#2e7d32
```

**Total: 5,739 lines across 26 files**

The three largest modules:
- **syscall.rs** (1,242) — Linux syscall emulation layer
- **sched.rs** (747) — SMP scheduler with futex, idle context, preemption
- **net/socket.rs** (565) — POSIX socket layer bridging ERTS to smoltcp
