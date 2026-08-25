/* include/urcu/config.h.  Generated from config.h.in by configure.  */
// SPDX-FileCopyrightText: 2023 EfficiOS Inc.
//
// SPDX-License-Identifier: LGPL-2.1-or-later

/* urcu/config.h.in. Manually generated for control over the contained defs. */

/* Enable SMP support. With SMP support enabled, uniprocessors are also
   supported. With SMP support disabled, UP systems work fine, but the
   behavior of SMP systems is undefined. */
#define CONFIG_RCU_SMP 1

/* TLS provided by the compiler. */
#define CONFIG_RCU_TLS 1

/* clock_gettime() is detected. */
#define CONFIG_RCU_HAVE_CLOCK_GETTIME 1

/* Require the operating system to support the membarrier system call for
   default and bulletproof flavors. */
/* #undef CONFIG_RCU_FORCE_SYS_MEMBARRIER */

/* Enable internal debugging self-checks.
   Introduces a performance penalty. */
/* #undef CONFIG_RCU_DEBUG */

/* Uatomic API uses atomic builtins. */
/* #undef CONFIG_RCU_USE_ATOMIC_BUILTINS */

/* Emit legacy memory barriers? */
#define CONFIG_RCU_EMIT_LEGACY_MB 1

/* Expose multi-flavor support */
#define CONFIG_RCU_HAVE_MULTIFLAVOR 1

/* Enable extra debugging checks for lock-free hash table iterator
   traversal. */
/* #undef CONFIG_CDS_LFHT_ITER_DEBUG */
