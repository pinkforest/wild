//#InputType: Object, Archive

#include "exit.h"

static int foo1 __attribute__ ((used, retain, section ("foo"))) = 2;
static int foo2 __attribute__ ((used, retain, section ("foo"))) = 5;

static int w1a __attribute__ ((used, retain, section ("w1"))) = 88;
static int w3a __attribute__ ((used, retain, section ("w3"))) = 88;

extern int __start_foo[];
extern int __stop_foo[];

// The `bar` section is only defined in our other file.
extern int __start_bar[];
extern int __stop_bar[];

extern int __start_w1[] __attribute__ ((weak));
extern int __stop_w1[] __attribute__ ((weak));
extern int __start_w2[] __attribute__ ((weak));
extern int __stop_w2[] __attribute__ ((weak));

// Override a symbol that would normally be created by the custom section.
int __stop_w3 = 88;

// Not really custom-section related, but also override a symbol that's normally defined by a
// built-in section.
int __init_array_start = 89;

int fn1(void);

int h1();
int h2(int x);

void _start(void) {
    int value = fn1();
    for (int *foo = __start_foo; foo < __stop_foo; foo++) {
        value += *foo;
    }
    for (int *bar = __start_bar; bar < __stop_bar; bar++) {
        value += *bar;
    }
    if (__start_w2 || __stop_w2) {
        exit_syscall(100);
    }
    if (__start_w1 == __stop_w1) {
        exit_syscall(101);
    }
    if (__start_w1[0] != 88) {
        exit_syscall(102);
    }
    if (h1() != 6) {
        exit_syscall(103);
    }
    if (h2(2) != 8) {
        exit_syscall(104);
    }
    if (__stop_w3 != 88) {
        exit_syscall(105);
    }
    if (__init_array_start != 89) {
        exit_syscall(106);
    }

    exit_syscall(value);
}
