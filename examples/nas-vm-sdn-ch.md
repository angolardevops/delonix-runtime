# NAS-VM na SDN via Cloud Hypervisor (100% rootless, sem libvirt)

Boot de uma VM na rede SDN delonix (sem firmware, sem AppArmor, sem qemu:///system),
com IP descobrível pela DHCP da SDN e alcançável por qualquer container na mesma rede.

## 1. Extrair kernel+initrd da cloud image (uma vez; sem download)
    export LIBGUESTFS_BACKEND=direct
    virt-get-kernel -a ~/.local/share/delonix/vm-images/delonix-vm-runtime-22.04-2026.06-amd64.qcow2 -o ./boot
    # → boot/vmlinuz-5.15.0-1101-kvm  (kernel `-kvm`, virtio built-in — ideal p/ CH)
    #   boot/initrd.img-5.15.0-1101-kvm

## 2. Boot na SDN (direct kernel boot — dispensa firmware)
    delonix vm create nasvm \
      --disk ~/.local/share/delonix/vm-images/delonix-vm-runtime-22.04-2026.06-amd64.qcow2 \
      --memory 1G --vcpus 2 \
      --backend cloud-hypervisor --network pnet \
      --kernel ./boot/vmlinuz-5.15.0-1101-kvm \
      --initrd ./boot/initrd.img-5.15.0-1101-kvm \
      --cmdline "console=ttyS0 root=/dev/vda1 rw" \
      --hostname nasvm --ssh-key @~/.ssh/id_ed25519.pub

    delonix vm status nasvm      # Running, ip: 10.219.x.x (DHCP da SDN)

## 3. Alcançar a VM (de qualquer container na mesma rede)
    delonix container run --net pnet -d --name probe alpine sleep 300
    delonix container exec probe ping -c2 <ip-da-vm>     # 0% loss
    delonix container exec probe nc -zv <ip-da-vm> 22    # open

## Notas
- O kernel `-kvm` do Ubuntu cloud tem virtio embutido → `root=/dev/vda1` sem módulos externos.
- `--backend libvirt` daria uma VM NÃO-SDN (rede própria do libvirt); a SDN é o caminho nativo.
- Para o HOST consumir um serviço da VM (ex.: SMB/NFS), publica-o pelo ingress (DNAT) — igual a um
  container; L3 dentro da SDN já está provado.
