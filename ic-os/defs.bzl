"""
A macro to build multiple versions of the ICOS image (i.e., dev vs prod)
"""

load("//toolchains/sysimage:toolchain.bzl", "disk_image", "docker_tar", "ext4_image", "sha256sum", "tar_extract", "upgrade_image")
load("//gitlab-ci/src/artifacts:upload.bzl", "upload_artifacts", "urls_test")
load("//bazel:output_files.bzl", "output_files")
load("@bazel_skylib//rules:copy_file.bzl", "copy_file")

img_bases = {
    "dev": "dfinity/guestos-base-dev@sha256:48f83b96fe53d82d028750593f21321984c19a40efeaada463d57609c592aef8",
    "prod": "dfinity/guestos-base@sha256:104fad74a45cd19419aa8abf15fc509d0bebfb971230a9810ddcd6e84b3bd856",
}

# Declare the dependencies that we will have for the built filesystem images.
# This needs to be done separately from the build rules because we want to
# compute the hash over all inputs going into the image and derive the
# "version.txt" file from it.
def _image_deps(mode, malicious = False):
    extra_rootfs_deps = {
        "dev": {"//ic-os/guestos:rootfs/allow_console_root": "/etc/allow_console_root:0644"},
        "prod": {},
    }
    deps = {
        "bootfs": {
            # base layer
            ":rootfs-tree.tar": "/",

            # We will install extra_boot_args onto the system, after substuting
            # the hash of the root filesystem into it. Add the template (before
            # substitution) as a dependency nevertheless such that changes
            # to the template file are reflected in the overall version hash
            # (the root_hash must include the version hash, it cannot be the
            # other way around).
            "//ic-os/guestos:bootloader/extra_boot_args.template": "/boot/extra_boot_args.template:0644",
        },
        "rootfs": {
            # base layer
            ":rootfs-tree.tar": "/",

            # additional files to install
            "//:canister_sandbox": "/opt/ic/bin/canister_sandbox:0755",
            "//:ic-btc-adapter": "/opt/ic/bin/ic-btc-adapter:0755",
            "//:ic-consensus-pool-util": "/opt/ic/bin/ic-consensus-pool-util:0755",
            "//:ic-canister-http-adapter": "/opt/ic/bin/ic-canister-http-adapter:0755",
            "//:ic-crypto-csp": "/opt/ic/bin/ic-crypto-csp:0755",
            "//:ic-regedit": "/opt/ic/bin/ic-regedit:0755",
            "//:ic-recovery": "/opt/ic/bin/ic-recovery:0755",
            "//:orchestrator": "/opt/ic/bin/orchestrator:0755",
            ("//:malicious_replica" if malicious else "//:replica"): "/opt/ic/bin/replica:0755",
            "//:sandbox_launcher": "/opt/ic/bin/sandbox_launcher:0755",
            "//:state-tool": "/opt/ic/bin/state-tool:0755",
            "//:vsock_agent": "/opt/ic/bin/vsock_agent:0755",
            "//ic-os/guestos/src:infogetty": "/opt/ic/bin/infogetty:0755",
            "//ic-os/guestos/src:prestorecon": "/opt/ic/bin/prestorecon:0755",
        },
    }
    deps["rootfs"].update(extra_rootfs_deps[mode])
    return deps

def icos_build(name, mode = None, malicious = False, visibility = None):
    """
    An ICOS build parameterized by mode.

    Args:
      name: Name for the generated filegroup.
      mode: dev or prod. If not specified, will use the value of `name`
      malicious: if True, bundle the `malicious_replica`
      visibility: See Bazel documentation
    """
    if mode == None:
        mode = name

    image_deps = _image_deps(mode, malicious)

    dev_rootfs_args = []

    if mode == "dev":
        dev_rootfs_args = ["--extra-dockerfile", "ic-os/guestos/rootfs/Dockerfile.dev", "--dev-root-ca", "ic-os/guestos/dev-root-ca.crt"]

    docker_tar(
        visibility = visibility,
        name = "rootfs-tree.tar",
        src = "//ic-os/guestos:rootfs",
        dep = native.glob(["rootfs/**"] + ["dev-root-ca.crt"] if mode == "dev" else []),
        extra_args_before = dev_rootfs_args,
        extra_args_after = [
            "--build-arg",
            "ROOT_PASSWORD=root",
            "--build-arg",
            "BASE_IMAGE=" + img_bases[mode],
        ],
        # The image is pretty big, therefore it is usually much faster to just rebuild it instead of fetching from the cache.
        # TODO(IDX-2221): remove this when CI jobs and bazel infrastructure will run in the same clusters.
        tags = ["no-remote-cache"],
        target_compatible_with = [
            "@platforms//os:linux",
        ],
    )

    ext4_image(
        name = "partition-config.tar",
        partition_size = "100M",
        target_compatible_with = [
            "@platforms//os:linux",
        ],
    )

    tar_extract(
        visibility = visibility,
        name = "file_contexts",
        src = "rootfs-tree.tar",
        path = "etc/selinux/default/contexts/files/file_contexts",
        target_compatible_with = [
            "@platforms//os:linux",
        ],
    )

    # TODO(IDX-2538): re-enable this (or any other similar) solution when everything will be ready to have ic version that is not git revision.
    #summary_sha256sum(
    #    name = "version.txt",
    #    inputs = image_deps,
    #    suffix = "-dev" if mode == "dev" else "",
    #)

    copy_file(
        name = "copy_version_txt",
        src = "//bazel:version.txt",
        out = "version.txt",
        allow_symlink = True,
    )

    copy_file(
        name = "copy_ic_version_id",
        src = ":version.txt",
        out = "ic_version_id",
        allow_symlink = True,
        visibility = ["//visibility:public"],
        tags = ["manual"],
    )

    ext4_image(
        name = "partition-boot.tar",
        src = "rootfs-tree.tar",
        # Take the dependency list declared above, and add in the "version.txt"
        # as well as the generated extra_boot_args file in the correct place.
        extra_files = {
            k: v
            for k, v in (
                image_deps["bootfs"].items() + [
                    ("version.txt", "/boot/version.txt:0644"),
                    ("extra_boot_args", "/boot/extra_boot_args:0644"),
                ]
            )
            if ":bootloader/extra_boot_args.template" not in k
            # additional files to install
            if v != "/"
        },
        file_contexts = ":file_contexts",
        partition_size = "1G",
        subdir = "boot/",
        target_compatible_with = [
            "@platforms//os:linux",
        ],
    )

    ext4_image(
        name = "partition-root-unsigned.tar",
        src = "rootfs-tree.tar",
        # Take the dependency list declared above, and add in the "version.txt"
        # at the correct place.
        extra_files = {
            k: v
            for k, v in (image_deps["rootfs"].items() + [(":version.txt", "/opt/ic/share/version.txt:0644")])
            if v != "/"
        },
        file_contexts = ":file_contexts",
        partition_size = "3G",
        strip_paths = [
            "/run",
            "/boot",
        ],
        # The image is pretty big, therefore it is usually much faster to just rebuild it instead of fetching from the cache.
        # TODO(IDX-2221): remove this when CI jobs and bazel infrastructure will run in the same clusters.
        tags = ["no-remote-cache"],
        target_compatible_with = [
            "@platforms//os:linux",
        ],
    )

    native.genrule(
        name = "partition-root-sign",
        srcs = ["partition-root-unsigned.tar"],
        outs = ["partition-root.tar", "partition-root-hash"],
        cmd = "$(location //toolchains/sysimage:verity_sign.py) -i $< -o $(location :partition-root.tar) -r $(location partition-root-hash)",
        executable = False,
        tools = ["//toolchains/sysimage:verity_sign.py"],
    )

    native.genrule(
        name = "extra_boot_args_root_hash",
        srcs = [
            "//ic-os/guestos:bootloader/extra_boot_args.template",
            ":partition-root-hash",
        ],
        outs = ["extra_boot_args"],
        cmd = "sed -e s/ROOT_HASH/$$(cat $(location :partition-root-hash))/ < $(location //ic-os/guestos:bootloader/extra_boot_args.template) > $@",
    )

    disk_image(
        name = "disk-img.tar",
        layout = "//ic-os/guestos:partitions.csv",
        partitions = [
            "//ic-os/bootloader:partition-esp.tar",
            "//ic-os/bootloader:partition-grub.tar",
            ":partition-config.tar",
            ":partition-boot.tar",
            "partition-root.tar",
        ],
        # The image is pretty big, therefore it is usually much faster to just rebuild it instead of fetching from the cache.
        # TODO(IDX-2221): remove this when CI jobs and bazel infrastructure will run in the same clusters.
        tags = ["no-remote-cache"],
        target_compatible_with = [
            "@platforms//os:linux",
        ],
    )

    upgrade_image(
        name = "upgrade.tar",
        boot_partition = ":partition-boot.tar",
        root_partition = ":partition-root.tar",
        # The image is pretty big, therefore it is usually much faster to just rebuild it instead of fetching from the cache.
        # TODO(IDX-2221): remove this when CI jobs and bazel infrastructure will run in the same clusters.
        tags = ["no-remote-cache"],
        target_compatible_with = [
            "@platforms//os:linux",
        ],
        version_file = ":version.txt",
    )

    native.genrule(
        name = "disk-img.tar_zst",
        srcs = ["disk-img.tar"],
        outs = ["disk-img.tar.zst"],
        cmd = "zstd --threads=0 -10 -f -z $< -o $@",
        # The image is pretty big, therefore it is usually much faster to just rebuild it instead of fetching from the cache.
        # TODO(IDX-2221): remove this when CI jobs and bazel infrastructure will run in the same clusters.
        tags = ["no-remote-cache"],
    )

    sha256sum(
        name = "disk-img.tar.zst.sha256",
        srcs = [":disk-img.tar.zst"],
    )

    native.genrule(
        name = "disk-img.tar_gz",
        srcs = ["disk-img.tar"],
        outs = ["disk-img.tar.gz"],
        cmd = "gzip --no-name -9 < $< > $@",
        # The image is pretty big, therefore it is usually much faster to just rebuild it instead of fetching from the cache.
        # TODO(IDX-2221): remove this when CI jobs and bazel infrastructure will run in the same clusters.
        tags = ["no-remote-cache"],
    )

    native.genrule(
        name = "upgrade.tar_zst",
        srcs = ["upgrade.tar"],
        outs = ["upgrade.tar.zst"],
        cmd = "zstd --threads=0 -10 -f -z $< -o $@",
        # The image is pretty big, therefore it is usually much faster to just rebuild it instead of fetching from the cache.
        # TODO(IDX-2221): remove this when CI jobs and bazel infrastructure will run in the same clusters.
        tags = ["no-remote-cache"],
    )

    sha256sum(
        name = "upgrade.tar.zst.sha256",
        srcs = [":upgrade.tar.zst"],
    )

    upload_suffix = ""
    if mode == "dev":
        upload_suffix = "-dev"
    if malicious:
        upload_suffix += "-malicious"

    upload_artifacts(
        name = "upload_disk-img",
        inputs = [
            ":disk-img.tar_zst",
            ":disk-img.tar_gz",
        ],
        remote_subdir = "guest-os/disk-img" + upload_suffix,
    )

    upload_artifacts(
        name = "upload_update-img",
        inputs = [
            ":upgrade.tar_zst",
        ],
        remote_subdir = "guest-os/update-img" + upload_suffix,
    )

    native.filegroup(
        name = "hash_and_upload_disk-img",
        srcs = [
            ":upload_disk-img",
            ":disk-img.tar.zst.sha256",
        ],
        visibility = ["//visibility:public"],
        tags = ["manual"],
    )

    native.filegroup(
        name = "hash_and_upload_upgrade-img",
        srcs = [
            ":upload_update-img",
            ":upgrade.tar.zst.sha256",
        ],
        visibility = ["//visibility:public"],
        tags = ["manual"],
    )

    urls_test(
        name = "upload_disk-img_test",
        inputs = [":upload_disk-img"],
    )

    output_files(
        name = "disk-img-url",
        target = ":upload_disk-img",
        basenames = ["upload_disk-img_disk-img.tar.zst.url"],
        tags = ["manual"],
    )

    native.py_binary(
        name = "launch_single_vm",
        main = "launch_single_vm.py",
        srcs = [
            "//ic-os/guestos:launch_single_vm.py",
            "//ic-os/guestos/tests:ictools.py",
        ],
        data = [
            ":disk-img.tar.zst.sha256",
            ":disk-img-url",
            ":version.txt",
            "//rs/prep:ic-prep",
        ],
        env = {
            "VERSION_FILE": "$(location :version.txt)",
            "IMG_HASH_FILE": "$(location :disk-img.tar.zst.sha256)",
            "DISK_IMG_URL_FILE": "$(location :disk-img-url)",
        },
        tags = ["manual"],
    )

    native.filegroup(
        name = name,
        srcs = [":disk-img.tar.zst", ":disk-img.tar.gz", ":upgrade.tar.zst"],
        visibility = visibility,
    )
