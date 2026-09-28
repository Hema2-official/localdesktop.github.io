#include <dirent.h>
#include <errno.h>
#include <linux/limits.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include "extension/fake_id0/unlink.h"
#include "extension/fake_id0/helper_functions.h"

/** Deletes the meta files in the directory at path whose file is gone. Renames
 *  used to leave them behind, and since tracees can't see them, removing the
 *  directory then failed with ENOTEMPTY no matter what they did.
 */
static void remove_orphaned_meta_files(const char path[PATH_MAX])
{
	DIR *dir;
	struct dirent *entry;
	struct stat statl;
	size_t tag_length = strlen(META_TAG);
	size_t suffix_length = strlen(META_SUFFIX);
	char file_path[PATH_MAX];
	char meta_path[PATH_MAX];

	dir = opendir(path);
	if(dir == NULL)
		return;

	while((entry = readdir(dir)) != NULL) {
		size_t length = strlen(entry->d_name);
		int status;

		if(length < tag_length + suffix_length
			|| strncmp(entry->d_name, META_TAG, tag_length) != 0
			|| strcmp(entry->d_name + length - suffix_length, META_SUFFIX) != 0)
			continue;

		/* META_TAG "<name>" META_SUFFIX belongs to "<name>"; one for
		 * an empty name (from paths with a trailing slash, see
		 * get_meta_path()) belongs to nothing.  */
		if(length > tag_length + suffix_length) {
			status = snprintf(file_path, PATH_MAX, "%s/%.*s", path,
				(int) (length - tag_length - suffix_length), entry->d_name + tag_length);
			if(status < 0 || status >= PATH_MAX)
				continue;
			if(lstat(file_path, &statl) == 0 || errno != ENOENT)
				continue;
		}

		status = snprintf(meta_path, PATH_MAX, "%s/%s", path, entry->d_name);
		if(status < 0 || status >= PATH_MAX)
			continue;
		unlink_meta(meta_path);
	}
	closedir(dir);
}

/** Handles unlink, unlinkat, and rmdir syscalls. Checks permissions in meta
 *  files matching the file to be unlinked if the meta file exists. Unlinks
 *  the meta file if the call would be successful. See unlink(2) and rmdir(2)
 *  for returned errors.
 */
int handle_unlink_enter_end(Tracee *tracee, Reg fd_sysarg, Reg path_sysarg, Config *config)
{
	int status;
	struct stat statl;
	char orig_path[PATH_MAX];
	char rel_path[PATH_MAX];
	char meta_path[PATH_MAX];

	status = read_sysarg_path(tracee, orig_path, path_sysarg, CURRENT);
	if(status < 0)
		return status;
	if(status == 1)
		return 0;

	status = get_meta_path(orig_path, meta_path);
	if(status < 0)
		return status;

	status = get_fd_path(tracee, rel_path, fd_sysarg, CURRENT);
	if(status < 0)
		return status;

	status = check_dir_perms(tracee, 'w', orig_path, rel_path, config);
	if(status < 0)
		return status;

	if(lstat(orig_path, &statl) == 0 && S_ISDIR(statl.st_mode))
		remove_orphaned_meta_files(orig_path);

	/** If the meta_file relating to the file being unlinked exists,
	 *  unlink that as well.
	 */
	if(path_exists(meta_path) == 0)
		unlink_meta(meta_path);

	return 0;
}
