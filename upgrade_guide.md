# Upgrade guide

1. Refer to the [upgrade notes](https://github.com/AppFlowy-IO/AppFlowy-SelfHost-Commercial/blob/main/README.md) (older versions [here](https://appflowy.com/docs/self-hosters-upgrade-notes)) for any important changes.

2. Sync this fork.

3. Fetch the tags from the upstream by
    ```
   git fetch --tags upstream
    ```
   
4. Merge the new version's tag into the `selfhosted` branch (ideally in an IDE). UPDATE: It seems AppFlowy cloud no longer utilizes tags for new releases, so just merge the `main` branch.

5. Check changes in the `.env` file manully, since it is ignored from Git.

5. Push to the origin.

6. On the server, pull from the origin.

7. Update the Docker image version variables in the .env file to the new versions published to [Docker Hub](https://hub.docker.com/u/appflowyinc).

8. Run
    ```
    docker compose up
    ```
   Do not rebuild the compose stack unless you really want to for some reason. It will take a long time. It is preferred
   to pull the prebuilt images.
