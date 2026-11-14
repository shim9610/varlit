#define _GNU_SOURCE
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>
#include <stdio.h>
#include <errno.h>
#include <time.h>
int main(int argc,char **argv){
 if(argc<4)return 125;
 struct timespec start,end;clock_gettime(CLOCK_MONOTONIC,&start);
 pid_t pid=fork();if(pid<0)return 125;
 if(!pid){execv(argv[3],argv+3);_exit(127);}
 FILE *p=fopen(argv[2],"w");if(!p)return 125;fprintf(p,"%d\n",pid);fclose(p);
 int status;struct rusage u;while(wait4(pid,&status,0,&u)<0)if(errno!=EINTR)return 125;
 clock_gettime(CLOCK_MONOTONIC,&end);
 double elapsed=end.tv_sec-start.tv_sec+(end.tv_nsec-start.tv_nsec)/1000000000.0;
 double user=u.ru_utime.tv_sec+u.ru_utime.tv_usec/1000000.0;
 double sys=u.ru_stime.tv_sec+u.ru_stime.tv_usec/1000000.0;
 FILE *o=fopen(argv[1],"w");if(!o)return 125;
 fprintf(o,"{\"elapsed_seconds\":%.9f,\"cpu\":%.9f,\"user_cpu\":%.9f,\"system_cpu\":%.9f,\"rss_kib\":%ld,\"write_bytes\":%ld,\"read_bytes\":%ld,\"minor_faults\":%ld,\"major_faults\":%ld,\"voluntary_switches\":%ld,\"involuntary_switches\":%ld}\n",elapsed,user+sys,user,sys,u.ru_maxrss,u.ru_oublock*512,u.ru_inblock*512,u.ru_minflt,u.ru_majflt,u.ru_nvcsw,u.ru_nivcsw);fclose(o);
 return WIFEXITED(status)?WEXITSTATUS(status):128+WTERMSIG(status);
}
