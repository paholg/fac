#ifndef POSIXMODEL_H
#define POSIXMODEL_H

#include <sys/types.h>

#include <stdbool.h>

enum inode_type {
  not_here = 0, is_file, is_directory, is_symlink, is_dir_or_symlink
};

struct inode {
  struct hash_entry e;
  enum inode_type type;
  struct inode *parent;
  bool is_written;
  bool is_read;
  union {
    struct hash_table children;
    char *readlink;
  } c; /* c for "contents" */
  char name[];
};

struct inode_pid_fd {
  struct inode *inode;
  pid_t pid;
  int fd;
};

struct posixmodel {
  struct inode *root;
  int num_fds;
  struct inode_pid_fd *open_stuff;
};

void init_posixmodel(struct posixmodel *m);

struct inode *lookup_fd(struct posixmodel *m, pid_t pid, int fd);

char *model_realpath(struct posixmodel *m, struct inode *i);
struct inode *model_cwd(struct posixmodel *m, pid_t pid);
struct inode *model_lstat(struct posixmodel *m, struct inode *cwd,
                          const char *path0);

int model_chdir(struct posixmodel *m, struct inode *cwd,
                const char *dir, pid_t pid);
int model_mkdir(struct posixmodel *m, struct inode *cwd, const char *dir);

int model_opendir(struct posixmodel *m, struct inode *cwd,
                  const char *dir, pid_t pid, int fd);
int model_readdir(struct posixmodel *m, pid_t pid, int fd);

#endif