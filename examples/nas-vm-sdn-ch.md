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

## 4. Storage SMB e2e — VM serve samba, container consome (PROVADO)

VM provisionada com samba por cloud-init (`--user-data <ficheiro>`, egress da SDN faz o apt):

    [global]
      map to guest = Bad User
      security = user
    [delonixnas]
      path = /srv/nas
      guest ok = yes
      read only = no
      force user = root

Como o `mount -t cifs` corre no netns do HOST, publica-se a porta SMB da VM para o host pelo
ingress e monta-se por `127.0.0.1`:

    delonix netns publish nasvm 4445:445 --ip <ip-da-vm>        # DNAT host:4445 → VM:445
    sudo -E delonix storage create nastest \                    # mount precisa de CAP_SYS_ADMIN
      --type cifs --server 127.0.0.1 --share delonixnas \
      --options "port=4445,guest,vers=3.0"
    sudo -E delonix container run -v nastest:/nas alpine \
      sh -c 'cat /nas/hello.txt; echo novo > /nas/from-container.txt'
      # LÊ o ficheiro criado dentro da VM E ESCREVE de volta — persistido na share da VM.

Fluxo: `VM(samba, SDN) --smb--> ingress:4445 --cifs--> Storage vol --bind--> container`.
Teardown liberta tudo (unpublish + storage rm desmonta; `--disk`/dados ficam): container rm,
storage rm, netns unpublish, vm rm.
