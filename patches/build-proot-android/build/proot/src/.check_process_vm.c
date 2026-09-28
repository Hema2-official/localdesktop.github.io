#include <sys/syscall.h>
#include <unistd.h>

/* Through syscall(2): Bionic only declares the wrappers from API 23, though the kernel has
 * the syscalls since Linux 3.2; tracee/mem.c provides the wrappers for older APIs.  */
int main(void)
{
	return syscall(SYS_process_vm_readv, 0, NULL, 0, NULL, 0, 0)
	       + syscall(SYS_process_vm_writev, 0, NULL, 0, NULL, 0, 0);
}
